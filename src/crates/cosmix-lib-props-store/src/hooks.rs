//! Namespace hooks per SPEC 12 §6.5.
//!
//! Every managed namespace registers a `HookHandler` with the substrate.
//! The default trait impls are no-ops; daemons override the methods they
//! need. The substrate library invokes them around the storage commit:
//!
//! 1. `before_set` runs first. Failure returns `validation_error` /
//!    `hook_error` and nothing else runs.
//! 2. Storage commit happens (record + event row in one tx).
//! 3. `after_set` runs synchronously, on the same task. Its return drives
//!    the Saga lifecycle flip (Ok → Active, Err → Failed).
//!
//! Delete mirrors the shape.
//!
//! `HookHandler` is intentionally object-safe and returns boxed futures
//! so the substrate can store `Arc<dyn HookHandler>` per namespace
//! without a generic parameter leak.
//!
//! ## Write origin
//!
//! [`WriteOrigin`] discriminates whether a set/delete came in over the
//! caller-facing Bus verb surface (`<svc>.props.set` / `.delete`) or
//! from a daemon-internal code path (bootstrap upsert, ergonomic
//! verb-shim, post-issuance writeback). Daemons that own server-side
//! fields (e.g. webd's `vhosts.cert_blob_id`) read
//! [`HookCtx::origin`] in their `before_set` hook to reject a caller
//! attempt to write a daemon-owned column. The unforgeability
//! guarantee lives in the type itself: only [`WriteOrigin::caller`]
//! and [`WriteOrigin::backend`] can construct values, and the
//! variant body is private — a caller-supplied `PropValue` carrying a
//! `"source":"bus_runtime"` field cannot turn into `Backend` without
//! going through `Runtime::set_with_origin(.., WriteOrigin::backend())`.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::record::{Actor, RecordKey, Version};
use crate::store::MergeMode;
use cosmix_props_core::value::PropValue;

/// SPEC 12 §6.5 hook failure modes. `Validation` maps to the `validation_error`
/// wire error (caller's request was malformed against the schema);
/// `Hook` maps to `hook_error` (the daemon's invariant was violated).
#[derive(Debug, Clone)]
pub enum HookError {
    Validation { message: String },
    Hook { message: String },
}

impl HookError {
    pub fn validation(msg: impl Into<String>) -> Self {
        Self::Validation {
            message: msg.into(),
        }
    }
    pub fn hook(msg: impl Into<String>) -> Self {
        Self::Hook {
            message: msg.into(),
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Self::Validation { message } | Self::Hook { message } => message,
        }
    }
}

impl fmt::Display for HookError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Validation { message } => write!(f, "validation_error: {message}"),
            Self::Hook { message } => write!(f, "hook_error: {message}"),
        }
    }
}

impl std::error::Error for HookError {}

pub type HookResult<T> = Result<T, HookError>;

/// Discriminator threaded into [`HookCtx::origin`] so hooks can
/// distinguish caller-facing Bus writes (`<svc>.props.set`,
/// `<svc>.props.delete`) from daemon-internal writes (bootstrap
/// upsert, ergonomic verb-shim, post-issuance writeback).
///
/// Unforgeability: the inner variant is private. The only ways to
/// produce a [`WriteOrigin`] are [`WriteOrigin::caller`] and
/// [`WriteOrigin::backend`]; a caller-supplied `PropValue` carrying
/// a `"source":"bus_runtime"` field cannot turn into `Backend`
/// without going through `Runtime::set_with_origin(..,
/// WriteOrigin::backend())`. Caller-facing Bus dispatch always
/// constructs `WriteOrigin::caller()`; only daemon-internal code
/// paths reach `WriteOrigin::backend()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteOrigin(WriteOriginInner);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteOriginInner {
    Caller,
    Backend,
}

impl WriteOrigin {
    /// The default origin for caller-facing Bus verb dispatch.
    pub const fn caller() -> Self {
        Self(WriteOriginInner::Caller)
    }

    /// Daemon-internal origin — used by `Runtime::set_with_origin` /
    /// `Runtime::delete_with_origin` for bootstrap upserts, ergonomic
    /// Bus verb shims, and post-issuance writebacks. A hook that
    /// guards daemon-owned columns checks `ctx.origin.is_backend()`
    /// before accepting writes to those columns.
    pub const fn backend() -> Self {
        Self(WriteOriginInner::Backend)
    }

    pub const fn is_caller(self) -> bool {
        matches!(self.0, WriteOriginInner::Caller)
    }

    pub const fn is_backend(self) -> bool {
        matches!(self.0, WriteOriginInner::Backend)
    }
}

impl Default for WriteOrigin {
    fn default() -> Self {
        Self::caller()
    }
}

/// Context passed to every hook. The substrate populates these from the
/// Bus request and the namespace registration; daemons read them
/// read-only.
#[derive(Debug, Clone)]
pub struct HookCtx {
    pub key: RecordKey,
    /// Previous value at this key, if any. `None` for create-new and for
    /// `before_set` calls where the prior row is absent (a hook that
    /// cares can short-circuit on `old.is_none()`).
    pub old: Option<PropValue>,
    /// Proposed new value (for set hooks) or final committed value (for
    /// after_set / delete hooks).
    pub new: Option<PropValue>,
    /// Version observed at hook invocation. For `before_set`, this is
    /// the prior version (zero if absent). For `after_set`, this is the
    /// just-committed version.
    pub version: Version,
    /// Who initiated the operation.
    pub actor: Actor,
    /// Merge mode for set hooks (`Some(MergeMode::Patch | Replace)`);
    /// `None` for delete hooks. Hooks that need to discriminate Replace
    /// vs Patch (e.g. to validate cross-field invariants against the
    /// merged effective value) branch on this. Hooks that don't care
    /// ignore it.
    pub merge: Option<MergeMode>,
    /// Write origin — `Caller` for Bus verb dispatch, `Backend` for
    /// daemon-internal writes via `Runtime::set_with_origin` /
    /// `Runtime::delete_with_origin`. Hooks that guard daemon-owned
    /// columns branch on `origin.is_backend()` to allow or reject the
    /// write.
    pub origin: WriteOrigin,
}

/// Convenience future alias for hook return types.
pub type HookFuture<'a, T> = Pin<Box<dyn Future<Output = HookResult<T>> + Send + 'a>>;

/// SPEC 12 §6.5 hook handler. All methods default to no-ops returning
/// `Ok(())`; daemons override what they need.
pub trait HookHandler: Send + Sync {
    fn before_set<'a>(&'a self, _ctx: &'a HookCtx) -> HookFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn after_set<'a>(&'a self, _ctx: &'a HookCtx) -> HookFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn before_delete<'a>(&'a self, _ctx: &'a HookCtx) -> HookFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn after_delete<'a>(&'a self, _ctx: &'a HookCtx) -> HookFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
}

/// Shared-ownership wrapper used by the namespace registry.
#[derive(Clone)]
pub struct Hooks(pub Arc<dyn HookHandler>);

impl Hooks {
    pub fn new<H: HookHandler + 'static>(h: H) -> Self {
        Self(Arc::new(h))
    }

    pub fn noop() -> Self {
        Self::new(NoopHooks)
    }
}

impl fmt::Debug for Hooks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Hooks").finish_non_exhaustive()
    }
}

/// Default no-op handler — sets/deletes always pass, no side effects.
pub struct NoopHooks;

impl HookHandler for NoopHooks {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::namespace::NamespaceName;

    fn ctx() -> HookCtx {
        HookCtx {
            key: RecordKey::collection(NamespaceName::new("accounts").unwrap(), "test@example.com"),
            old: None,
            new: Some(PropValue::from("x")),
            version: Version::zero(),
            actor: Actor::operator("delta").expect("valid actor"),
            merge: Some(MergeMode::Replace),
            origin: WriteOrigin::caller(),
        }
    }

    struct RejectsBlank;
    impl HookHandler for RejectsBlank {
        fn before_set<'a>(&'a self, ctx: &'a HookCtx) -> HookFuture<'a, ()> {
            Box::pin(async move {
                match &ctx.new {
                    Some(PropValue::String(s)) if s.is_empty() => {
                        Err(HookError::validation("value must not be blank"))
                    }
                    _ => Ok(()),
                }
            })
        }
    }

    #[tokio::test]
    async fn noop_passes() {
        let h: Box<dyn HookHandler> = Box::new(NoopHooks);
        assert!(h.before_set(&ctx()).await.is_ok());
        assert!(h.after_set(&ctx()).await.is_ok());
        assert!(h.before_delete(&ctx()).await.is_ok());
        assert!(h.after_delete(&ctx()).await.is_ok());
    }

    #[tokio::test]
    async fn custom_rejects_blank() {
        let h = Hooks::new(RejectsBlank);
        let mut c = ctx();
        c.new = Some(PropValue::from(""));
        let err = h.0.before_set(&c).await.unwrap_err();
        assert!(matches!(err, HookError::Validation { .. }));
        assert!(err.message().contains("blank"));
    }

    #[test]
    fn hook_error_display() {
        let e = HookError::validation("bad email");
        assert_eq!(e.to_string(), "validation_error: bad email");
        let e = HookError::hook("partner service down");
        assert_eq!(e.to_string(), "hook_error: partner service down");
    }

    #[test]
    fn write_origin_default_is_caller() {
        // Default must be caller; otherwise existing callers of
        // `HookCtx { .. }` literal-construction silently flip from
        // caller to backend at upgrade time.
        let o: WriteOrigin = Default::default();
        assert!(o.is_caller());
        assert!(!o.is_backend());
    }

    #[test]
    fn write_origin_backend_is_distinct() {
        let c = WriteOrigin::caller();
        let b = WriteOrigin::backend();
        assert_ne!(c, b);
        assert!(b.is_backend());
        assert!(!b.is_caller());
    }
}
