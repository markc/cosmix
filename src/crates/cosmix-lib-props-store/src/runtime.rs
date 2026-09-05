//! Substrate runtime — threads hooks + store for Simple and Saga lifecycles.
//!
//! The runtime is the in-process layer the Bus verbs (C4) call into. For
//! each `<svc>.props.set` / `<svc>.props.delete`, it:
//!
//! 1. Runs `before_set` / `before_delete` against the resolved
//!    [`HookCtx`]. A hook error short-circuits before any storage write
//!    and surfaces as `validation_error` / `hook_error` per SPEC 12 §9.
//! 2. Commits the record + event row via the [`PropertyStore`]. Saga
//!    namespaces stamp the inbound row with `Lifecycle::Provisioning`;
//!    Simple namespaces stamp `None`.
//! 3. Runs `after_set` / `after_delete`. For Simple namespaces, an
//!    `after_set` failure is appended to the response body as a
//!    sanitised entry in [`SetOutcome::warnings`]; the record is durable
//!    and the call returns `Ok` (SPEC 12 §6.5 line 1066-1068). Only
//!    `before_*` hook failures surface as `hook_error` to the caller.
//! 4. For Saga namespaces, an `after_set` `Ok(())` triggers a
//!    `commit_complete(Active)`; an `Err` triggers
//!    `commit_complete(Failed{reason})`. The Provisioning record stays
//!    durable for operator inspection in the failure case (§6.5).
//!
//! **Hook re-entrancy.** Hooks run with no locks held by the runtime
//! itself. They may call into the same store via the public Bus surface
//! without deadlock; concurrent commits serialise inside the store
//! according to its own contract (e.g. [`crate::memory::MemoryStore`]'s
//! per-instance mutex).
//!
//! **Same-key Saga concurrency model.** SPEC 12 §4.4 is optimistic, not
//! pessimistic — the runtime does NOT serialise the full
//! `set → after_set → complete` sequence per key. If a second writer
//! advances the row while writer A's `after_set` is in flight, A's
//! `commit_complete` surfaces [`StoreError::VersionMismatch`] (the
//! version check runs before the lifecycle precondition in
//! [`crate::memory::MemoryStore::commit_complete`] so the diagnostic
//! reflects the displacement, not a substrate misuse). The corollary —
//! `after_set` side effects (e.g. provisioning external resources) may
//! have run for a row that another writer has now displaced — is the
//! orphan-recovery story handled by SPEC 12 §11 reconciliation, which
//! lands with the sqlite backend in C3. Daemons must make `after_set`
//! idempotent (per §11) so a subsequent reconciliation replay can
//! re-run it cleanly.
//!
//! **Hook author contract for failure messages.** SPEC 12 line 601 says
//! the Saga `Failed { reason }` value is the `after_set` error string
//! "secret-redacted by the sanitiser". The substrate runs
//! `sanitize_reason` (private to this module) over the message before
//! persisting it — that
//! strips ASCII control bytes (so ANSI escapes / CSI sequences cannot
//! survive into the audit stream) and caps length. It does NOT semantic-
//! redact application secrets. Hook authors MUST NOT include secret
//! material (passwords, tokens, raw HTTP bodies from upstream) in
//! `HookError` messages — those would survive sanitisation. The
//! schema-aware secret-field redactor that lands with C5 protects the
//! record `value` projection; the saga reason string is a separate
//! channel and is the hook author's responsibility.

use std::fmt;
use std::sync::Arc;

use tokio::sync::Notify;

use crate::hooks::{HookCtx, HookError, HookHandler, WriteOrigin};
use crate::lifecycle::Lifecycle;
use crate::namespace::{NamespaceLifecycle, NamespaceSpec};
use crate::record::{Actor, RecordEvent, RecordKey, Version};
use crate::store::{CompleteCommit, DeleteCommit, MergeMode, PropertyStore, SetCommit, StoreError};
use cosmix_props_core::value::PropValue;

/// Errors surfaced to the Bus verb layer. Each maps 1:1 to a SPEC 12 §9
/// wire `error_code`; the verb layer constructs the body envelope.
#[derive(Debug, Clone)]
pub enum RuntimeError {
    /// A `before_*` or `after_*` hook returned a [`HookError`]. The
    /// inner variant carries the `validation_error` vs `hook_error`
    /// distinction per §6.5.
    Hook(HookError),
    /// Storage-layer error, propagated verbatim.
    Store(StoreError),
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hook(e) => write!(f, "{e}"),
            Self::Store(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RuntimeError {}

impl From<HookError> for RuntimeError {
    fn from(e: HookError) -> Self {
        Self::Hook(e)
    }
}

impl From<StoreError> for RuntimeError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

/// Caller-supplied options for `Runtime::set`. The fields named after
/// SPEC 12 §8.2 wire headers (`expected_version`, `merge`, `cause`,
/// `actor`) match the values the verb layer extracts from the request.
#[derive(Debug, Clone)]
pub struct SetOpts {
    pub expected_version: Option<Version>,
    pub merge: MergeMode,
    pub actor: Actor,
    pub cause: Option<String>,
    pub ts_ms: i64,
}

/// Caller-supplied options for `Runtime::delete`.
#[derive(Debug, Clone)]
pub struct DeleteOpts {
    pub expected_version: Option<Version>,
    pub actor: Actor,
    pub cause: Option<String>,
    pub ts_ms: i64,
}

/// Result of a successful `Runtime::set`. For Simple namespaces
/// `complete_event` is always `None`; for Saga it carries the
/// `Completed` event the substrate emitted after `after_set` ran.
///
/// `warnings` carries `after_set` failures from Simple namespaces. SPEC 12
/// §6.5 line 1066-1068: "For `Simple` namespaces, `after_set` failures are
/// logged and appended to the response body as a `warnings` array; they
/// do not change the response's success status." Saga `after_set` failures
/// drive the lifecycle to `Failed{reason}` instead and do not populate
/// this field.
#[derive(Debug, Clone)]
pub struct SetOutcome {
    pub set_event: RecordEvent,
    pub complete_event: Option<RecordEvent>,
    pub warnings: Vec<String>,
}

/// Per-namespace coordinator. Owns the [`NamespaceSpec`] (hooks, lifecycle
/// mode, validators) and an `Arc` to the shared store. Constructed at
/// daemon-startup namespace-registration time; held for the lifetime of
/// the daemon.
///
/// **Live-event signal.** Each successful commit (set, delete, and the
/// Saga complete that follows a successful set's `after_set`) fires
/// [`events_signal`](Self::events_signal) via `notify_one`. The fan-out
/// [`crate::dispatcher::spawn_dispatcher`] parks on this notify and, on
/// wakeup, drains `events_since` against the namespace and publishes
/// the projected `<svc>.props.records.changed` + `<svc>.props.audit`
/// topics. `notify_one` stores a permit if no waiter is currently
/// parked, so a commit that fires while the dispatcher is mid-drain is
/// consumed by the next `notified()` call — closing the
/// drain-then-park race. Notifications coalesce: multiple commits
/// between two wake-ups produce one dispatcher iteration, which then
/// reads every newer `nseq` in a single pass.
#[derive(Clone)]
pub struct Runtime {
    service: String,
    spec: NamespaceSpec,
    store: Arc<dyn PropertyStore>,
    events_signal: Arc<Notify>,
}

impl Runtime {
    pub fn new(
        service: impl Into<String>,
        spec: NamespaceSpec,
        store: Arc<dyn PropertyStore>,
    ) -> Self {
        Self {
            service: service.into(),
            spec,
            store,
            events_signal: Arc::new(Notify::new()),
        }
    }

    pub fn service(&self) -> &str {
        &self.service
    }

    pub fn spec(&self) -> &NamespaceSpec {
        &self.spec
    }

    pub fn store(&self) -> &Arc<dyn PropertyStore> {
        &self.store
    }

    /// Handle to the per-runtime live-events notify. Cloned by the
    /// fan-out [`crate::dispatcher::spawn_dispatcher`] at construction so the
    /// dispatcher task can park on `notified()` without holding a
    /// reference to the runtime. Every successful commit_set /
    /// commit_delete / commit_complete on this runtime fires the
    /// notify via `notify_one`, which **stores a permit** if no waiter
    /// is parked — closing the drain-then-park race where a commit
    /// fires after the dispatcher reads events but before it re-parks
    /// on `notified()`. Coalescing still holds: a second `notify_one`
    /// while one permit is already stored is a no-op, so multiple
    /// commits between two wake-ups produce one dispatcher iteration.
    pub fn events_signal(&self) -> Arc<Notify> {
        Arc::clone(&self.events_signal)
    }

    /// Single namespace `<svc>.props.set` operation. Returns the durable
    /// set event always; on Saga namespaces also returns the `Completed`
    /// event the substrate emitted after `after_set`.
    ///
    /// Caller-facing dispatch — origin is stamped `WriteOrigin::caller()`.
    /// Daemon-internal code paths (bootstrap upsert, ergonomic verb shim,
    /// post-issuance writeback) call [`Runtime::set_with_origin`] with
    /// `WriteOrigin::backend()` so hooks guarding daemon-owned columns
    /// can branch on `ctx.origin.is_backend()`.
    pub async fn set(
        &self,
        key: RecordKey,
        value: PropValue,
        opts: SetOpts,
    ) -> Result<SetOutcome, RuntimeError> {
        self.set_with_origin(key, value, opts, WriteOrigin::caller())
            .await
    }

    /// Same as [`Runtime::set`] but allows the caller to declare the
    /// write origin. The substrate threads `origin` into [`HookCtx`] so
    /// hooks can distinguish caller-facing writes from daemon-internal
    /// ones. The unforgeability guarantee lives in [`WriteOrigin`]
    /// itself — there is no way for a caller-facing Bus path to
    /// construct `WriteOrigin::backend()` without explicit code change.
    pub async fn set_with_origin(
        &self,
        key: RecordKey,
        value: PropValue,
        opts: SetOpts,
        origin: WriteOrigin,
    ) -> Result<SetOutcome, RuntimeError> {
        self.validate_key(&key)?;
        // Validate completion attribution before hooks or the first saga write.
        // A bad owning service must not leave a committed provisioning row.
        let complete_actor = if matches!(self.spec.lifecycle, NamespaceLifecycle::Saga) {
            Some(
                Actor::daemon_complete(&self.service)
                    .map_err(|e| HookError::validation(format!("completion actor: {e}")))?,
            )
        } else {
            None
        };
        // SPEC 12 §4.4 — `require_version: true` makes `if_version`
        // mandatory. Refuse before any IO or hook side effects.
        if self.spec.require_version && opts.expected_version.is_none() {
            return Err(RuntimeError::Hook(HookError::validation(
                "namespace requires if_version on set",
            )));
        }

        // Snapshot the prior record for hook context; `NotFound` means
        // "create-new".
        let prior = match self.store.get(&key).await {
            Ok(snap) => Some(snap.value),
            Err(StoreError::NotFound) => None,
            Err(other) => return Err(other.into()),
        };
        let prior_value = prior.as_ref().map(|r| r.value.clone());
        let prior_version = prior.as_ref().map(|r| r.version).unwrap_or(Version::zero());

        // Primary-key shape needs to know whether this is a create,
        // replace, or patch-against-existing — checked after the prior
        // fetch but before any hooks fire.
        self.validate_primary_key(&key, &value, opts.merge, prior.is_some())?;

        // SPEC 12 §6.1 — pure validators run before `before_set`. They
        // are synchronous, IO-free, and fail with `validation_error`.
        // Iterate in declaration order; first failure short-circuits.
        for validator in &self.spec.validators {
            (validator.check)(&value)?;
        }

        let ctx = HookCtx {
            key: key.clone(),
            old: prior_value.clone(),
            new: Some(value.clone()),
            version: prior_version,
            actor: opts.actor.clone(),
            merge: Some(opts.merge),
            origin,
        };
        self.hooks().before_set(&ctx).await?;

        // Saga records carry Provisioning on the initial row; Simple
        // records carry None.
        let initial_lifecycle = match self.spec.lifecycle {
            NamespaceLifecycle::Saga => Some(Lifecycle::Provisioning),
            NamespaceLifecycle::Simple => None,
        };

        let (record, set_event) = self
            .store
            .commit_set(SetCommit {
                key: key.clone(),
                new_value: value,
                expected_version: opts.expected_version,
                merge: opts.merge,
                lifecycle: initial_lifecycle,
                actor: opts.actor.clone(),
                cause: opts.cause.clone(),
                ts_ms: opts.ts_ms,
            })
            .await?;
        // The set event is durable; wake the dispatcher so subscribers
        // see the live `records.changed` + audit row in the same wall
        // clock tick. Saga `commit_complete` below fires its own notify.
        self.events_signal.notify_one();

        let after_ctx = HookCtx {
            key: key.clone(),
            old: prior_value,
            new: Some(record.value.clone()),
            version: record.version,
            actor: opts.actor.clone(),
            merge: Some(opts.merge),
            origin,
        };
        let after_result = self.hooks().after_set(&after_ctx).await;

        match self.spec.lifecycle {
            NamespaceLifecycle::Simple => {
                // SPEC 12 §6.5 line 1066-1068: Simple `after_set` failures
                // are logged + surfaced as `warnings`; the record stays
                // durable and the response is `success`. The failure does
                // NOT bubble as `hook_error` (that contract is for
                // `before_*` only).
                let warnings = match after_result {
                    Ok(()) => Vec::new(),
                    Err(e) => vec![sanitize_reason(e.message())],
                };
                Ok(SetOutcome {
                    set_event,
                    complete_event: None,
                    warnings,
                })
            }
            NamespaceLifecycle::Saga => {
                let complete_lifecycle = match after_result {
                    Ok(()) => Lifecycle::Active,
                    // SPEC 12 §6.6 line 601: the reason is "secret-redacted
                    // by the sanitiser". The schema-aware sanitiser arrives
                    // with C5; for C2 we apply a defensive transform that
                    // strips control bytes and caps length so a noisy
                    // hook error cannot smuggle ANSI escapes / huge blobs
                    // into the audit-visible Failed reason.
                    Err(e) => Lifecycle::Failed {
                        reason: sanitize_reason(e.message()),
                    },
                };
                // Pin the complete to the version this saga's commit_set
                // produced; a concurrent writer that already advanced
                // the row turns this into `VersionMismatch` rather than
                // overwriting their state with our stale outcome.
                let (_record, complete_event) = self
                    .store
                    .commit_complete(CompleteCommit {
                        key,
                        lifecycle: complete_lifecycle,
                        expected_version: record.version,
                        actor: complete_actor
                            .expect("saga completion actor validated before write"),
                        cause: opts.cause,
                        ts_ms: opts.ts_ms,
                    })
                    .await?;
                // Saga complete is a second durable event on the same
                // key; wake the dispatcher again so subscribers see
                // both the set and the lifecycle transition.
                self.events_signal.notify_one();
                Ok(SetOutcome {
                    set_event,
                    complete_event: Some(complete_event),
                    warnings: Vec::new(),
                })
            }
        }
    }

    /// Single namespace `<svc>.props.delete` operation. Returns the
    /// tombstone event. Hook errors short-circuit before the storage
    /// write.
    ///
    /// Caller-facing dispatch — origin is stamped `WriteOrigin::caller()`.
    /// Daemon-internal code paths call [`Runtime::delete_with_origin`]
    /// with `WriteOrigin::backend()`.
    pub async fn delete(
        &self,
        key: RecordKey,
        opts: DeleteOpts,
    ) -> Result<RecordEvent, RuntimeError> {
        self.delete_with_origin(key, opts, WriteOrigin::caller())
            .await
    }

    /// Same as [`Runtime::delete`] but allows the caller to declare
    /// the write origin. See [`Runtime::set_with_origin`] for the
    /// unforgeability rationale.
    pub async fn delete_with_origin(
        &self,
        key: RecordKey,
        opts: DeleteOpts,
        origin: WriteOrigin,
    ) -> Result<RecordEvent, RuntimeError> {
        self.validate_key(&key)?;
        // SPEC 12 §4.4 — `require_version: true` applies to delete too.
        if self.spec.require_version && opts.expected_version.is_none() {
            return Err(RuntimeError::Hook(HookError::validation(
                "namespace requires if_version on delete",
            )));
        }

        let prior = self.store.get(&key).await?;

        let ctx = HookCtx {
            key: key.clone(),
            old: Some(prior.value.value.clone()),
            new: None,
            version: prior.value.version,
            actor: opts.actor.clone(),
            merge: None,
            origin,
        };
        self.hooks().before_delete(&ctx).await?;

        let event = self
            .store
            .commit_delete(DeleteCommit {
                key: key.clone(),
                expected_version: opts.expected_version,
                actor: opts.actor.clone(),
                cause: opts.cause,
                ts_ms: opts.ts_ms,
            })
            .await?;
        // Tombstone is durable; wake the dispatcher so subscribers
        // see the live `records.changed` deletion event.
        self.events_signal.notify_one();

        let after_ctx = HookCtx {
            key,
            old: Some(prior.value.value),
            new: None,
            version: event.version,
            actor: opts.actor,
            merge: None,
            origin,
        };
        self.hooks().after_delete(&after_ctx).await?;

        Ok(event)
    }

    fn hooks(&self) -> &dyn HookHandler {
        // `Hooks` wraps `Arc<dyn HookHandler>` (see hooks.rs); deref once
        // to a borrowed trait object.
        &*self.spec.hooks.0
    }

    /// Defensive trust-boundary check at the runtime → store seam. A
    /// `Runtime` instance owns exactly one namespace's policy (hooks,
    /// lifecycle, validators, require_version, cardinality). Storage is
    /// commonly shared across all namespaces for one daemon, so a key
    /// addressing a different namespace would silently apply this
    /// runtime's policy to a row outside its policy domain. Reject before
    /// any IO, hook, or validator side effects. Also enforces the §6.2
    /// cardinality key-shape contract: Singleton keys are the empty
    /// string (per §6.2 "the wire `key:` header is the empty string");
    /// Collection keys are non-empty.
    fn validate_key(&self, key: &RecordKey) -> Result<(), RuntimeError> {
        if key.namespace != self.spec.name {
            return Err(RuntimeError::Hook(HookError::validation(format!(
                "key namespace {:?} does not match runtime namespace {:?}",
                key.namespace.as_str(),
                self.spec.name.as_str(),
            ))));
        }
        // Sub-path mutation is not a runtime primitive. The storage
        // layer rejects this too, but doing it here short-circuits
        // validators, hooks, and any prior-record fetch.
        if !key.is_whole_record() {
            return Err(RuntimeError::Hook(HookError::validation(
                "non-empty `field_path` is not supported on set/delete",
            )));
        }
        match &self.spec.cardinality {
            crate::namespace::Cardinality::Singleton { .. } => {
                if !key.key.is_empty() {
                    return Err(RuntimeError::Hook(HookError::validation(
                        "Singleton namespace requires empty `key`",
                    )));
                }
            }
            crate::namespace::Cardinality::Collection { .. } => {
                if key.key.is_empty() {
                    return Err(RuntimeError::Hook(HookError::validation(
                        "Collection namespace requires non-empty `key`",
                    )));
                }
            }
        }
        Ok(())
    }

    /// SPEC 12 §6.2 `Collection { primary_key_field }`: the substrate
    /// owns the contract that the wire `key:` and the value's
    /// `primary_key_field` agree.
    ///
    /// - Create (prior absent) or Replace (any prior): the value MUST
    ///   be an object containing `primary_key_field` as a String equal
    ///   to `key.key`. A create or replace that omits the pk field would
    ///   either store a row with no canonical key field (create) or drop
    ///   it from an existing row (replace).
    /// - Patch against an existing row: the field may be omitted (the
    ///   prior value's pk is preserved). If present, must still equal
    ///   `key.key`.
    /// - Singleton namespaces and non-object root values: no-op (SPEC
    ///   12 §4.3 allows scalar/list root values; pk only applies to
    ///   Collection over Object).
    fn validate_primary_key(
        &self,
        key: &RecordKey,
        value: &PropValue,
        merge: MergeMode,
        prior_exists: bool,
    ) -> Result<(), RuntimeError> {
        let pk_field = match &self.spec.cardinality {
            crate::namespace::Cardinality::Collection { primary_key_field } => primary_key_field,
            crate::namespace::Cardinality::Singleton { .. } => return Ok(()),
        };
        let obj = match value {
            PropValue::Object(o) => o,
            _ => return Ok(()),
        };
        let pk_required = matches!(merge, MergeMode::Replace)
            || (matches!(merge, MergeMode::Patch) && !prior_exists);
        match obj.get(pk_field) {
            Some(PropValue::String(s)) if s == &key.key => Ok(()),
            Some(_) => Err(RuntimeError::Hook(HookError::validation(format!(
                "value.{pk_field:?} must equal record key {:?}",
                key.key
            )))),
            None if pk_required => Err(RuntimeError::Hook(HookError::validation(format!(
                "value must carry {pk_field:?} equal to record key on create/replace",
            )))),
            None => Ok(()),
        }
    }
}

/// Maximum length of a Saga `Failed.reason` / Simple warning string. Cap
/// chosen to give operators enough context for triage while keeping the
/// audit row bounded — long error blobs are usually evidence of a hook
/// echoing a stack trace or an external response body, both of which can
/// carry secrets.
const REASON_MAX_LEN: usize = 256;

/// Defensive sanitiser applied to `after_set` failure messages before
/// they are written to lifecycle state or surfaced as warnings. SPEC 12
/// line 601 mandates the reason be "secret-redacted by the sanitiser";
/// the schema-aware secret-field redactor lands with C5 alongside the
/// audit HMAC. Until then this strips ASCII control bytes (so ANSI / CSI
/// sequences cannot survive into the audit stream), replaces other
/// non-printables with `?`, and caps length. Pure / synchronous / no
/// allocation beyond the result string.
fn sanitize_reason(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(REASON_MAX_LEN));
    for ch in s.chars() {
        if out.len() >= REASON_MAX_LEN {
            out.push('…');
            break;
        }
        match ch {
            '\t' | ' ' => out.push(ch),
            c if c.is_ascii_graphic() => out.push(c),
            c if c.is_alphanumeric() => out.push(c), // non-ASCII letters
            _ => out.push('?'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::{HookFuture, HookHandler, Hooks};
    use crate::memory::MemoryStore;
    use crate::namespace::{
        Cardinality, NamespaceName, NamespaceSpec, PropertySchema, StorageBackendKind,
    };
    use crate::record::{EventKind, Nseq};
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn ns() -> NamespaceName {
        NamespaceName::new("accounts").unwrap()
    }

    fn obj(pairs: &[(&str, PropValue)]) -> PropValue {
        let mut m = BTreeMap::new();
        for (k, v) in pairs {
            m.insert((*k).to_string(), v.clone());
        }
        PropValue::Object(m)
    }

    fn spec_simple() -> NamespaceSpec {
        NamespaceSpec::new(
            ns(),
            PropertySchema::default(),
            Cardinality::Collection {
                primary_key_field: "email".into(),
            },
            StorageBackendKind::Memory,
        )
    }

    fn spec_saga() -> NamespaceSpec {
        let mut s = spec_simple();
        s.lifecycle = NamespaceLifecycle::Saga;
        s
    }

    fn set_opts() -> SetOpts {
        SetOpts {
            expected_version: Some(Version::zero()),
            merge: MergeMode::Patch,
            actor: Actor::operator("markc").expect("valid actor"),
            cause: None,
            ts_ms: 1_700_000_000_000,
        }
    }

    #[tokio::test]
    async fn simple_set_records_no_lifecycle() {
        let store: Arc<dyn PropertyStore> = Arc::new(MemoryStore::new("maild"));
        let rt = Runtime::new("maild", spec_simple(), Arc::clone(&store));
        let key = RecordKey::collection(ns(), "u@h");
        let out = rt
            .set(
                key.clone(),
                obj(&[("email", PropValue::from("u@h"))]),
                set_opts(),
            )
            .await
            .unwrap();
        assert_eq!(out.set_event.kind, EventKind::Created);
        assert!(out.set_event.lifecycle.is_none());
        assert!(out.complete_event.is_none());

        let stored = store.get(&key).await.unwrap();
        assert!(stored.value.lifecycle.is_none());
    }

    /// Hook fixture that flips success/failure based on a counter so
    /// individual saga calls can drive their own outcome.
    struct CountingHooks {
        fail_after_calls: u32,
        calls: AtomicU32,
    }

    impl HookHandler for CountingHooks {
        fn after_set<'a>(&'a self, _ctx: &'a HookCtx) -> HookFuture<'a, ()> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            let fail = self.fail_after_calls;
            Box::pin(async move {
                if n > fail {
                    Ok(())
                } else {
                    Err(HookError::hook(format!("seed failed on call {n}")))
                }
            })
        }
    }

    #[tokio::test]
    async fn invalid_completion_actor_refused_before_saga_write() {
        let store: Arc<dyn PropertyStore> = Arc::new(MemoryStore::new("maild"));
        let rt = Runtime::new("bad service", spec_saga(), Arc::clone(&store));
        let key = RecordKey::collection(ns(), "alice@example.org");
        let error = rt
            .set(
                key.clone(),
                obj(&[("email", PropValue::from("alice@example.org"))]),
                set_opts(),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("completion actor"));
        assert!(matches!(store.get(&key).await, Err(StoreError::NotFound)));
        assert!(
            store
                .events_since(&ns(), crate::record::Nseq::zero())
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn saga_happy_path_flips_to_active() {
        let store: Arc<dyn PropertyStore> = Arc::new(MemoryStore::new("maild"));
        let mut spec = spec_saga();
        // 0 means "never fail" — after_set always returns Ok.
        spec.hooks = Hooks::new(CountingHooks {
            fail_after_calls: 0,
            calls: AtomicU32::new(0),
        });
        let rt = Runtime::new("maild", spec, Arc::clone(&store));

        let key = RecordKey::collection(ns(), "u@h");
        let out = rt
            .set(
                key.clone(),
                obj(&[("email", PropValue::from("u@h"))]),
                set_opts(),
            )
            .await
            .unwrap();

        assert_eq!(out.set_event.kind, EventKind::Created);
        assert_eq!(out.set_event.lifecycle, Some(Lifecycle::Provisioning));
        let complete = out.complete_event.expect("saga emits complete event");
        assert_eq!(complete.kind, EventKind::Completed);
        assert_eq!(complete.lifecycle, Some(Lifecycle::Active));
        assert_eq!(complete.actor.as_str(), "daemon:maild");
        assert!(complete.nseq > out.set_event.nseq);

        // Record stored as Active.
        let stored = store.get(&key).await.unwrap();
        assert_eq!(stored.value.lifecycle, Some(Lifecycle::Active));
    }

    #[tokio::test]
    async fn saga_failure_flips_to_failed_with_reason() {
        let store: Arc<dyn PropertyStore> = Arc::new(MemoryStore::new("maild"));
        let mut spec = spec_saga();
        // Fail on the first call.
        spec.hooks = Hooks::new(CountingHooks {
            fail_after_calls: 1,
            calls: AtomicU32::new(0),
        });
        let rt = Runtime::new("maild", spec, Arc::clone(&store));

        let key = RecordKey::collection(ns(), "u@h");
        let out = rt
            .set(
                key.clone(),
                obj(&[("email", PropValue::from("u@h"))]),
                set_opts(),
            )
            .await
            .unwrap();
        let complete = out.complete_event.expect("saga emits complete on failure");
        match complete.lifecycle {
            Some(Lifecycle::Failed { ref reason }) => {
                assert!(reason.contains("seed failed"), "got {reason}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        let stored = store.get(&key).await.unwrap();
        assert!(matches!(
            stored.value.lifecycle,
            Some(Lifecycle::Failed { .. })
        ));
    }

    /// before_set rejection MUST short-circuit before any storage write.
    struct RejectingHooks;
    impl HookHandler for RejectingHooks {
        fn before_set<'a>(&'a self, _ctx: &'a HookCtx) -> HookFuture<'a, ()> {
            Box::pin(async { Err(HookError::validation("nope")) })
        }
    }

    #[tokio::test]
    async fn before_set_rejection_short_circuits() {
        let store: Arc<dyn PropertyStore> = Arc::new(MemoryStore::new("maild"));
        let mut spec = spec_simple();
        spec.hooks = Hooks::new(RejectingHooks);
        let rt = Runtime::new("maild", spec, Arc::clone(&store));

        let key = RecordKey::collection(ns(), "u@h");
        let err = rt
            .set(
                key.clone(),
                obj(&[("email", PropValue::from("u@h"))]),
                set_opts(),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            RuntimeError::Hook(HookError::Validation { .. })
        ));

        // Nothing committed.
        let snap = store.list(&ns()).await.unwrap();
        assert!(snap.value.is_empty());
        let events = store.events_since(&ns(), Nseq::zero()).await.unwrap();
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn delete_runs_before_and_after_hooks() {
        let store: Arc<dyn PropertyStore> = Arc::new(MemoryStore::new("maild"));
        let rt = Runtime::new("maild", spec_simple(), Arc::clone(&store));
        let key = RecordKey::collection(ns(), "u@h");

        rt.set(
            key.clone(),
            obj(&[("email", PropValue::from("u@h"))]),
            set_opts(),
        )
        .await
        .unwrap();

        let event = rt
            .delete(
                key.clone(),
                DeleteOpts {
                    expected_version: Some(Version(1)),
                    actor: Actor::operator("markc").expect("valid actor"),
                    cause: None,
                    ts_ms: 0,
                },
            )
            .await
            .unwrap();
        assert_eq!(event.kind, EventKind::Deleted);
        // Record gone.
        let err = store.get(&key).await.unwrap_err();
        assert!(matches!(err, StoreError::NotFound));
    }

    /// Ordering invariant: events from concurrent `Runtime::set` calls on
    /// the same namespace appear in the store's event log in
    /// strictly-monotonic nseq order, and for Saga namespaces each set
    /// is immediately followed by its complete event (the runtime drives
    /// commit_set → after_set → commit_complete as a single async
    /// sequence; the store serialises individual commits, but interleavings
    /// of full set+complete sequences are permitted).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn saga_concurrent_sets_ordering() {
        use crate::namespace::NamespaceLifecycle;

        let store: Arc<dyn PropertyStore> = Arc::new(MemoryStore::new("maild"));
        let mut spec = spec_simple();
        spec.lifecycle = NamespaceLifecycle::Saga;
        spec.hooks = Hooks::new(CountingHooks {
            fail_after_calls: 0,
            calls: AtomicU32::new(0),
        });
        let rt = Arc::new(Runtime::new("maild", spec, Arc::clone(&store)));

        let mut handles = Vec::new();
        for i in 0..8u64 {
            let rt = Arc::clone(&rt);
            handles.push(tokio::spawn(async move {
                let email = format!("u{i}");
                let key = RecordKey::collection(ns(), email.clone());
                rt.set(key, obj(&[("email", PropValue::from(email))]), set_opts())
                    .await
                    .unwrap()
            }));
        }
        let outcomes: Vec<SetOutcome> = futures_join_all(handles).await.into_iter().collect();
        assert_eq!(outcomes.len(), 8);
        assert!(outcomes.iter().all(|o| o.complete_event.is_some()));

        let events = store.events_since(&ns(), Nseq::zero()).await.unwrap();
        // 2 events per saga set (Created + Completed) × 8 calls = 16.
        assert_eq!(events.len(), 16);
        // Strictly monotonic nseq.
        for (i, ev) in events.iter().enumerate() {
            assert_eq!(ev.nseq, Nseq((i as u64) + 1));
        }
        // Every Completed event reaches Active (no failures configured).
        let completed: Vec<_> = events
            .iter()
            .filter(|e| e.kind == EventKind::Completed)
            .collect();
        assert_eq!(completed.len(), 8);
        assert!(
            completed
                .iter()
                .all(|e| matches!(e.lifecycle, Some(Lifecycle::Active)))
        );
    }

    // Minimal join-all so we don't pull `futures` just for tests.
    async fn futures_join_all<T>(handles: Vec<tokio::task::JoinHandle<T>>) -> Vec<T> {
        let mut out = Vec::with_capacity(handles.len());
        for h in handles {
            out.push(h.await.unwrap());
        }
        out
    }

    /// Validators MUST run before `before_set` and short-circuit on the
    /// first failure. SPEC 12 §6.1 — failure is `validation_error`, the
    /// record is never written, and no hooks fire.
    #[tokio::test]
    async fn validator_rejects_before_any_storage_write() {
        use crate::namespace::Validator;

        let store: Arc<dyn PropertyStore> = Arc::new(MemoryStore::new("maild"));
        let mut spec = spec_simple();
        let validator = Validator::new(
            "email_required",
            Arc::new(|v: &PropValue| match v {
                PropValue::Object(m) if m.contains_key("email") => Ok(()),
                _ => Err(HookError::validation("email required")),
            }),
        );
        spec.validators = vec![validator];
        // Wire a hook that would panic if reached — if before_set runs,
        // the test fails loudly rather than silently passing.
        struct PanickyHooks;
        impl HookHandler for PanickyHooks {
            fn before_set<'a>(&'a self, _ctx: &'a HookCtx) -> HookFuture<'a, ()> {
                Box::pin(async { panic!("before_set must not run when validator rejects") })
            }
        }
        spec.hooks = Hooks::new(PanickyHooks);
        let rt = Runtime::new("maild", spec, Arc::clone(&store));

        let key = RecordKey::collection(ns(), "u@h");
        let err = rt
            .set(
                key,
                obj(&[("display", PropValue::from("no email field"))]),
                set_opts(),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            RuntimeError::Hook(HookError::Validation { .. })
        ));
        // Nothing durable.
        let snap = store.list(&ns()).await.unwrap();
        assert!(snap.value.is_empty());
    }

    /// SPEC 12 §6.5 line 1066-1068: Simple `after_set` failures surface
    /// as `warnings`, not `hook_error`. The record stays durable, the
    /// response is success.
    #[tokio::test]
    async fn simple_after_set_failure_returns_warning_not_error() {
        let store: Arc<dyn PropertyStore> = Arc::new(MemoryStore::new("maild"));
        let mut spec = spec_simple();
        spec.hooks = Hooks::new(CountingHooks {
            fail_after_calls: 1,
            calls: AtomicU32::new(0),
        });
        let rt = Runtime::new("maild", spec, Arc::clone(&store));

        let key = RecordKey::collection(ns(), "u@h");
        let out = rt
            .set(
                key.clone(),
                obj(&[("email", PropValue::from("u@h"))]),
                set_opts(),
            )
            .await
            .expect("Simple after_set Err MUST NOT bubble as RuntimeError");
        assert!(out.complete_event.is_none());
        assert_eq!(out.warnings.len(), 1);
        assert!(out.warnings[0].contains("seed failed"));
        // Record is durable despite the warning.
        let stored = store.get(&key).await.unwrap();
        assert!(stored.value.lifecycle.is_none());
    }

    /// SPEC 12 line 601: Saga `Failed.reason` is "secret-redacted by the
    /// sanitiser". The C2 defensive transform strips ASCII control bytes
    /// so an ANSI / CSI sequence cannot survive into the audit stream.
    #[tokio::test]
    async fn saga_failed_reason_strips_control_bytes() {
        struct AnsiHooks;
        impl HookHandler for AnsiHooks {
            fn after_set<'a>(&'a self, _ctx: &'a HookCtx) -> HookFuture<'a, ()> {
                Box::pin(async { Err(HookError::hook("\x1b[31mleak\x1b[0m\x07\nplain")) })
            }
        }

        let store: Arc<dyn PropertyStore> = Arc::new(MemoryStore::new("maild"));
        let mut spec = spec_saga();
        spec.hooks = Hooks::new(AnsiHooks);
        let rt = Runtime::new("maild", spec, Arc::clone(&store));

        let key = RecordKey::collection(ns(), "u@h");
        let out = rt
            .set(key, obj(&[("email", PropValue::from("u@h"))]), set_opts())
            .await
            .unwrap();
        let reason = match out.complete_event.unwrap().lifecycle {
            Some(Lifecycle::Failed { reason }) => reason,
            other => panic!("expected Failed, got {other:?}"),
        };
        // Esc / BEL / newline must all be neutralised — no raw control
        // bytes survive.
        assert!(!reason.contains('\x1b'), "ESC survived: {reason:?}");
        assert!(!reason.contains('\x07'), "BEL survived: {reason:?}");
        assert!(!reason.contains('\n'), "newline survived: {reason:?}");
        assert!(reason.contains("leak"));
        assert!(reason.contains("plain"));
    }

    /// SPEC 12 §4.4: `require_version: true` makes `if_version` mandatory.
    /// Calls without `expected_version` MUST be refused before any
    /// storage write or hook side effect.
    #[tokio::test]
    async fn require_version_set_refused_without_if_version() {
        let store: Arc<dyn PropertyStore> = Arc::new(MemoryStore::new("maild"));
        let mut spec = spec_simple();
        spec.require_version = true;
        let rt = Runtime::new("maild", spec, Arc::clone(&store));

        let key = RecordKey::collection(ns(), "u@h");
        let err = rt
            .set(
                key,
                obj(&[("email", PropValue::from("u@h"))]),
                SetOpts {
                    expected_version: None,
                    ..set_opts()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            RuntimeError::Hook(HookError::Validation { .. })
        ));
        // Nothing committed.
        let snap = store.list(&ns()).await.unwrap();
        assert!(snap.value.is_empty());
    }

    #[tokio::test]
    async fn require_version_delete_refused_without_if_version() {
        let store: Arc<dyn PropertyStore> = Arc::new(MemoryStore::new("maild"));
        let mut spec = spec_simple();
        spec.require_version = true;
        let rt = Runtime::new("maild", spec, Arc::clone(&store));

        // Seed a record (use a no-require_version runtime so we can plant
        // a row, then test delete against the same store with the
        // require_version-on runtime).
        let seed_spec = spec_simple();
        let seed_rt = Runtime::new("maild", seed_spec, Arc::clone(&store));
        let key = RecordKey::collection(ns(), "u@h");
        seed_rt
            .set(
                key.clone(),
                obj(&[("email", PropValue::from("u@h"))]),
                set_opts(),
            )
            .await
            .unwrap();

        let err = rt
            .delete(
                key,
                DeleteOpts {
                    expected_version: None,
                    actor: Actor::operator("markc").expect("valid actor"),
                    cause: None,
                    ts_ms: 0,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            RuntimeError::Hook(HookError::Validation { .. })
        ));
    }

    /// Same-key saga concurrency: when a second writer advances the row
    /// between the first writer's `commit_set` and `commit_complete`,
    /// the first writer's complete MUST surface `VersionMismatch` rather
    /// than overwriting the second writer's row. Side-effect orphan
    /// recovery is §11 reconciliation (C3+); for C2 we pin the
    /// expected-version contract.
    ///
    /// Deterministic schedule: writer A blocks in its first `after_set`
    /// on a "row-committed" gate (so its `Provisioning` commit_set is
    /// durable); main task then directly runs writer B (whose hook does
    /// not block, racing to advance the row past A); main task then
    /// notifies A, which unblocks and tries to `commit_complete` against
    /// the stale version.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn saga_same_key_race_surfaces_version_mismatch() {
        use tokio::sync::Notify;

        let store: Arc<dyn PropertyStore> = Arc::new(MemoryStore::new("maild"));
        let mut spec = spec_saga();
        let advance_gate = Arc::new(Notify::new());

        struct GatedHooks {
            advance_gate: Arc<Notify>,
            calls: AtomicU32,
        }
        impl HookHandler for GatedHooks {
            fn after_set<'a>(&'a self, _ctx: &'a HookCtx) -> HookFuture<'a, ()> {
                let n = self.calls.fetch_add(1, Ordering::SeqCst);
                let gate = Arc::clone(&self.advance_gate);
                Box::pin(async move {
                    // First caller (writer A) parks until the main task
                    // notifies that B has advanced the row.
                    if n == 0 {
                        gate.notified().await;
                    }
                    Ok(())
                })
            }
        }
        spec.hooks = Hooks::new(GatedHooks {
            advance_gate: Arc::clone(&advance_gate),
            calls: AtomicU32::new(0),
        });
        let rt = Arc::new(Runtime::new("maild", spec, Arc::clone(&store)));
        let key = RecordKey::collection(ns(), "u@h");

        // Spawn writer A; it blocks inside after_set holding nothing.
        let rt_a = Arc::clone(&rt);
        let key_a = key.clone();
        let writer_a = tokio::spawn(async move {
            rt_a.set(
                key_a,
                obj(&[
                    ("email", PropValue::from("u@h")),
                    ("display", PropValue::from("A")),
                ]),
                set_opts(),
            )
            .await
        });

        // Park until A's commit_set has landed (row at Version(1)).
        // Use a small sleep rather than yield_now so the second worker
        // is genuinely free to schedule A.
        let mut landed = false;
        for _ in 0..200 {
            if let Ok(snap) = store.get(&key).await
                && snap.value.version == Version(1)
            {
                landed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(landed, "A's commit_set must be visible before B races");

        // Writer B runs inline on the main task. Its hook is the SECOND
        // call (no block); commit_set advances v1→v2 (Provisioning), then
        // commit_complete advances v2→v3 (Active). A is still parked.
        let b_out = rt
            .set(
                key.clone(),
                obj(&[
                    ("email", PropValue::from("u@h")),
                    ("display", PropValue::from("B")),
                ]),
                SetOpts {
                    expected_version: Some(Version(1)),
                    ..set_opts()
                },
            )
            .await
            .unwrap();
        assert!(b_out.complete_event.is_some());
        assert_eq!(store.get(&key).await.unwrap().value.version, Version(3));

        // Now release A; its commit_complete (expected_version=Version(1))
        // must observe current=Version(3) and surface VersionMismatch.
        advance_gate.notify_one();
        let a_result = writer_a.await.unwrap();
        match a_result {
            Err(RuntimeError::Store(StoreError::VersionMismatch { expected, current })) => {
                assert_eq!(expected, Version(1));
                assert_eq!(current, Version(3));
            }
            other => panic!("expected VersionMismatch for A, got {other:?}"),
        }
    }

    /// Trust-boundary check: a `Runtime` owns exactly one namespace.
    /// A key for a different namespace MUST be refused before any IO so
    /// the runtime's hooks/lifecycle/require_version cannot be applied
    /// to a row outside its policy domain (memory and sqlite backends
    /// commonly host multiple namespaces in one store).
    #[tokio::test]
    async fn key_for_other_namespace_refused() {
        let store: Arc<dyn PropertyStore> = Arc::new(MemoryStore::new("maild"));
        let rt = Runtime::new("maild", spec_simple(), Arc::clone(&store));

        let other = NamespaceName::new("themes").unwrap();
        let key = RecordKey::collection(other, "u@h");
        let err = rt
            .set(key, obj(&[("email", PropValue::from("u@h"))]), set_opts())
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            RuntimeError::Hook(HookError::Validation { .. })
        ));
    }

    /// `Runtime` must reject sub-path keys BEFORE running validators or
    /// hooks. The storage layer rejects them too, but the short-circuit
    /// here avoids any side effects from hook code paths.
    #[tokio::test]
    async fn field_path_rejected_before_hooks() {
        use crate::record::FieldPathSegment;

        struct PanickyHooks;
        impl HookHandler for PanickyHooks {
            fn before_set<'a>(&'a self, _ctx: &'a HookCtx) -> HookFuture<'a, ()> {
                Box::pin(async { panic!("hooks must not run on sub-path keys") })
            }
        }

        let store: Arc<dyn PropertyStore> = Arc::new(MemoryStore::new("maild"));
        let mut spec = spec_simple();
        spec.hooks = Hooks::new(PanickyHooks);
        let rt = Runtime::new("maild", spec, Arc::clone(&store));

        let key = RecordKey::collection(ns(), "u@h")
            .with_field_path(vec![FieldPathSegment::field("display")]);
        let err = rt
            .set(key, PropValue::from("new display"), set_opts())
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            RuntimeError::Hook(HookError::Validation { .. })
        ));
    }

    /// SPEC 12 §6.2 — for `Collection { primary_key_field }`:
    /// (a) value carrying a divergent pk field is refused,
    /// (b) create (prior absent) without the pk field is refused,
    /// (c) replace without the pk field is refused,
    /// (d) patch against an existing row may omit the pk field.
    #[tokio::test]
    async fn collection_primary_key_contract() {
        let store: Arc<dyn PropertyStore> = Arc::new(MemoryStore::new("maild"));
        let rt = Runtime::new("maild", spec_simple(), Arc::clone(&store));

        let key = RecordKey::collection(ns(), "alice@example.com");
        // (a) divergent pk.
        let err = rt
            .set(
                key.clone(),
                obj(&[("email", PropValue::from("bob@example.com"))]),
                set_opts(),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            RuntimeError::Hook(HookError::Validation { .. })
        ));

        // (b) create without pk field.
        let err = rt
            .set(
                key.clone(),
                obj(&[("display", PropValue::from("Alice"))]),
                set_opts(),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            RuntimeError::Hook(HookError::Validation { .. })
        ));

        // Seed a valid row.
        rt.set(
            key.clone(),
            obj(&[("email", PropValue::from("alice@example.com"))]),
            set_opts(),
        )
        .await
        .unwrap();

        // (c) replace without pk field is refused.
        let err = rt
            .set(
                key.clone(),
                obj(&[("display", PropValue::from("Alice"))]),
                SetOpts {
                    expected_version: Some(Version(1)),
                    merge: MergeMode::Replace,
                    ..set_opts()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            RuntimeError::Hook(HookError::Validation { .. })
        ));

        // (d) patch against existing row may omit pk field.
        rt.set(
            key,
            obj(&[("display", PropValue::from("Alice"))]),
            SetOpts {
                expected_version: Some(Version(1)),
                ..set_opts()
            },
        )
        .await
        .unwrap();
    }

    /// Singleton namespace MUST refuse non-empty `key` strings; the wire
    /// `key:` is the empty string per SPEC 12 §6.2.
    #[tokio::test]
    async fn singleton_namespace_rejects_non_empty_key() {
        let store: Arc<dyn PropertyStore> = Arc::new(MemoryStore::new("maild"));
        let mut spec = NamespaceSpec::new(
            ns(),
            PropertySchema::default(),
            Cardinality::Singleton {
                canonical_key: "_singleton".into(),
            },
            StorageBackendKind::Memory,
        );
        // Disable require_version to isolate the cardinality check.
        spec.require_version = false;
        let rt = Runtime::new("maild", spec, Arc::clone(&store));

        let key = RecordKey::collection(ns(), "u@h");
        let err = rt
            .set(key, obj(&[("v", PropValue::from(1_i64))]), set_opts())
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            RuntimeError::Hook(HookError::Validation { .. })
        ));
    }

    /// Sub-path mutation isn't a storage primitive; commit_set / delete
    /// must refuse a non-empty `field_path` to keep the §6.5 reserved-
    /// field guard meaningful.
    #[tokio::test]
    async fn commit_set_rejects_non_empty_field_path() {
        use crate::record::FieldPathSegment;
        use crate::store::{MergeMode, SetCommit};

        let store = MemoryStore::new("maild");
        let key = RecordKey::collection(ns(), "u@h")
            .with_field_path(vec![FieldPathSegment::field("inner")]);
        let err = store
            .commit_set(SetCommit {
                key,
                new_value: PropValue::from("v"),
                expected_version: Some(Version::zero()),
                merge: MergeMode::Patch,
                lifecycle: None,
                actor: Actor::service("maild").expect("valid actor"),
                cause: None,
                ts_ms: 0,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Validation { .. }));
    }

    /// `Runtime::set` (no `_with_origin` suffix) must stamp
    /// `WriteOrigin::caller()` into the hook context. Daemons that adopt
    /// the daemon-owned-column pattern (e.g. webd's vhost
    /// `cert_blob_id` / `source` fields) rely on this default — a
    /// silent flip to `Backend` would let any caller-facing Bus write
    /// bypass the hook's `is_backend()` guard.
    #[tokio::test]
    async fn write_origin_caller_is_default() {
        use std::sync::Mutex;

        struct OriginRecorder {
            seen: Arc<Mutex<Vec<WriteOrigin>>>,
        }
        impl HookHandler for OriginRecorder {
            fn before_set<'a>(&'a self, ctx: &'a HookCtx) -> HookFuture<'a, ()> {
                self.seen.lock().unwrap().push(ctx.origin);
                Box::pin(async { Ok(()) })
            }
        }

        let store: Arc<dyn PropertyStore> = Arc::new(MemoryStore::new("maild"));
        let mut spec = spec_simple();
        let seen = Arc::new(Mutex::new(Vec::new()));
        spec.hooks = Hooks::new(OriginRecorder {
            seen: Arc::clone(&seen),
        });
        let rt = Runtime::new("maild", spec, Arc::clone(&store));

        rt.set(
            RecordKey::collection(ns(), "u@h"),
            obj(&[("email", PropValue::from("u@h"))]),
            set_opts(),
        )
        .await
        .unwrap();

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert!(
            seen[0].is_caller(),
            "Runtime::set must stamp caller origin; got {:?}",
            seen[0]
        );
    }

    /// `Runtime::set_with_origin(.., WriteOrigin::backend())` must
    /// thread the backend origin into both `before_set` and `after_set`
    /// hook contexts. This is the load-bearing invariant Phase 3
    /// daemon-owned columns depend on — a webd hook reading
    /// `ctx.origin.is_backend()` must see the daemon-internal write
    /// as backend, not caller.
    #[tokio::test]
    async fn write_origin_backend_threads_into_hook_ctx() {
        use std::sync::Mutex;

        struct OriginRecorder {
            before: Arc<Mutex<Vec<WriteOrigin>>>,
            after: Arc<Mutex<Vec<WriteOrigin>>>,
        }
        impl HookHandler for OriginRecorder {
            fn before_set<'a>(&'a self, ctx: &'a HookCtx) -> HookFuture<'a, ()> {
                self.before.lock().unwrap().push(ctx.origin);
                Box::pin(async { Ok(()) })
            }
            fn after_set<'a>(&'a self, ctx: &'a HookCtx) -> HookFuture<'a, ()> {
                self.after.lock().unwrap().push(ctx.origin);
                Box::pin(async { Ok(()) })
            }
        }

        let store: Arc<dyn PropertyStore> = Arc::new(MemoryStore::new("maild"));
        let mut spec = spec_simple();
        let before = Arc::new(Mutex::new(Vec::new()));
        let after = Arc::new(Mutex::new(Vec::new()));
        spec.hooks = Hooks::new(OriginRecorder {
            before: Arc::clone(&before),
            after: Arc::clone(&after),
        });
        let rt = Runtime::new("maild", spec, Arc::clone(&store));

        rt.set_with_origin(
            RecordKey::collection(ns(), "u@h"),
            obj(&[("email", PropValue::from("u@h"))]),
            set_opts(),
            WriteOrigin::backend(),
        )
        .await
        .unwrap();

        let before = before.lock().unwrap();
        let after = after.lock().unwrap();
        assert_eq!(before.len(), 1);
        assert_eq!(after.len(), 1);
        assert!(
            before[0].is_backend(),
            "before_set must see backend origin; got {:?}",
            before[0]
        );
        assert!(
            after[0].is_backend(),
            "after_set must see backend origin; got {:?}",
            after[0]
        );
    }

    /// Symmetric guarantee for `delete_with_origin`: backend origin
    /// reaches both `before_delete` and `after_delete` hooks.
    #[tokio::test]
    async fn delete_with_origin_backend_threads_into_hook_ctx() {
        use std::sync::Mutex;

        struct OriginRecorder {
            before: Arc<Mutex<Vec<WriteOrigin>>>,
            after: Arc<Mutex<Vec<WriteOrigin>>>,
        }
        impl HookHandler for OriginRecorder {
            fn before_delete<'a>(&'a self, ctx: &'a HookCtx) -> HookFuture<'a, ()> {
                self.before.lock().unwrap().push(ctx.origin);
                Box::pin(async { Ok(()) })
            }
            fn after_delete<'a>(&'a self, ctx: &'a HookCtx) -> HookFuture<'a, ()> {
                self.after.lock().unwrap().push(ctx.origin);
                Box::pin(async { Ok(()) })
            }
        }

        let store: Arc<dyn PropertyStore> = Arc::new(MemoryStore::new("maild"));
        let mut spec = spec_simple();
        let before = Arc::new(Mutex::new(Vec::new()));
        let after = Arc::new(Mutex::new(Vec::new()));
        spec.hooks = Hooks::new(OriginRecorder {
            before: Arc::clone(&before),
            after: Arc::clone(&after),
        });
        let rt = Runtime::new("maild", spec, Arc::clone(&store));

        // Seed a row first so delete has something to remove.
        let key = RecordKey::collection(ns(), "u@h");
        rt.set(
            key.clone(),
            obj(&[("email", PropValue::from("u@h"))]),
            set_opts(),
        )
        .await
        .unwrap();

        rt.delete_with_origin(
            key,
            DeleteOpts {
                expected_version: None,
                actor: Actor::operator("markc").expect("valid actor"),
                cause: None,
                ts_ms: 1_700_000_000_000,
            },
            WriteOrigin::backend(),
        )
        .await
        .unwrap();

        let before = before.lock().unwrap();
        let after = after.lock().unwrap();
        assert_eq!(before.len(), 1);
        assert_eq!(after.len(), 1);
        assert!(before[0].is_backend());
        assert!(after[0].is_backend());
    }
}
