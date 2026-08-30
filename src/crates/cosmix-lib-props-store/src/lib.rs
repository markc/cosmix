//! Property surface for cosmix daemons.
//!
//! # Crate-pair layout (post 2026-05-29 split)
//!
//! Since the 2026-05-29 lib-props split (see
//! `_doc/planned/2026-05-29-lib-props-split.md`) the property surface
//! is a pair of crates:
//!
//! - **[`cosmix-lib-props-core`](::cosmix_props_core)** owns the SPEC 07
//!   §2 read surface: `PropTree`, `PropPath`, `PropValue`,
//!   `PropDescribe`, `redact`, `diff`, plus the `dispatch_props` Bus
//!   wire and `publish` builders behind its `bus` feature. New
//!   consumers that only need the read surface should depend on
//!   `cosmix-lib-props-core` directly and import these symbols from
//!   there.
//! - **This crate (`cosmix-lib-props-store`)** owns the SPEC 12 §§5–8
//!   mutation surface: `NamespaceSpec`, `Lifecycle`, `RecordKey`,
//!   `Record`, `RecordEvent`, `Hooks`, `Capability`, `PropertyStore`,
//!   the in-memory and SQLite backends, the `Runtime` coordinator,
//!   per-row audit HMAC, the broker-reserved fan-out dispatcher, and
//!   the SPEC 12 mutation router. It also re-exports every read-surface
//!   symbol from `cosmix-lib-props-core` (and carries
//!   module-path-preserving shims at `src/{describe,diff,path,redact,
//!   tree,value}.rs`) so pre-split consumers continue to resolve
//!   `cosmix_props::PropPath`, `cosmix_props::value::PropValue`, etc.
//!   without source-level changes.
//!
//! The split is a code-organization change, not a wire-contract change;
//! SPEC 07 / SPEC 12 invariants are preserved across the pair.

pub mod audit;
pub mod capability;
pub mod describe;
pub mod diff;
pub mod hooks;
pub mod lifecycle;
pub mod memory;
pub mod namespace;
pub mod path;
pub mod record;
pub mod redact;
pub mod runtime;
#[cfg(feature = "sqlite")]
pub mod sqlite;
pub mod store;
pub mod tree;
pub mod value;

#[cfg(feature = "cosmix")]
pub mod bus;

#[cfg(feature = "cosmix")]
pub use bus::mutation::PropsRouter;

#[cfg(feature = "cosmix")]
pub mod dispatcher;

#[cfg(feature = "cosmix")]
pub use dispatcher::{
    DispatchPublisher, DispatcherHandle, audit_topic, records_changed_topic, spawn_dispatcher,
};

#[cfg(feature = "cosmix")]
pub mod subscribe_granter;

#[cfg(feature = "cosmix")]
pub use subscribe_granter::SubscribeGranter;

#[cfg(feature = "cosmix")]
pub mod publish;

// SPEC 07 read surface — these symbols live in `cosmix-lib-props-core`
// since the 2026-05-29 lib-props split. The shim module files
// (`src/{describe,diff,path,redact,tree,value}.rs`) one-line re-export
// from core, and these re-exports give consumers the same
// `cosmix_props::PropPath` etc. paths they used pre-split. See the
// crate-level rustdoc above for the full crate-pair layout and the
// preferred dep direction for new consumer code.
pub use describe::{PropDescribe, PropType};
pub use diff::diff;
pub use path::{PropPath, PropPathError};
pub use redact::redact;
pub use tree::PropTree;
pub use value::PropValue;

// SPEC 12 mutation surface — new in v0.2.
pub use audit::{AuditKey, canonical_serialise, compute_digest, tombstone_value};
pub use capability::{AuthDecision, Capability, CapabilitySet};
pub use hooks::{
    HookCtx, HookError, HookFuture, HookHandler, HookResult, Hooks, NoopHooks, WriteOrigin,
};
pub use lifecycle::{LIFECYCLE_FIELD, Lifecycle};
pub use memory::MemoryStore;
pub use namespace::{
    AuthPolicy, Cardinality, ConformanceLevel, DeleteMode, ExternalEditPolicy, FieldSchema,
    FieldType, NamespaceLifecycle, NamespaceName, NamespaceNameError, NamespaceSpec, PeerIdentity,
    PropertySchema, ReplayWindow, SchemaPublic, SchemaVersion, StorageBackendKind,
    SubscribePayload, Validator, ValidatorFn,
};
pub use record::{
    Actor, AuditEpoch, AuditRow, EventKind, FieldPathSegment, Nseq, Record, RecordEvent, RecordKey,
    Version,
};
pub use runtime::{DeleteOpts, Runtime, RuntimeError, SetOpts, SetOutcome};
pub use store::{
    CompleteCommit, DeleteCommit, MergeMode, PropertyStore, ReconcileCommit, SetCommit, Snapshot,
    StoreError, StoreFuture, StoreResult,
};

#[cfg(feature = "sqlite")]
pub use sqlite::{JsonValuesMapping, SqliteStore, SqliteTableMapping};
