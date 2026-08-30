//! `cosmix-lib-log-props` — the SPEC-12 `<svc>.log` namespace plus the
//! live-reload watcher that drives a `cosmix_log` subscriber's
//! `EnvFilter` from substrate writes.
//!
//! This is a **cos extension** of the `cosmix_log` logging core (which
//! lives in the bus repo). The core owns the subscriber, sinks, and the
//! `LogReloadHandle`; this crate owns the agent-operable surface: a
//! daemon registers the `<svc>.log` namespace against its `PropsRouter`
//! and `SqliteStore`, then calls [`attach_props`] to start a watcher
//! that swaps the live filter every time an operator writes a new level
//! via `<svc>.props.set`.
//!
//! Only daemons that own a `PropsRouter` and `SqliteStore` (webd, maild
//! today) wire this; everyone else just uses `cosmix_log::init`.

mod log_attach;
mod log_namespace;

pub use log_attach::attach_props;
pub use log_namespace::{
    CANONICAL_KEY, INVALID_FILTER_PREFIX, LogNamespaceHooks, LogPropsRuntime, NAMESPACE,
    UNDELETABLE_PREFIX, UNKNOWN_ENUM_VARIANT_PREFIX, applied_by_variants, auth_policy,
    format_variants, level_variants, namespace_name, register_log_namespace, schema, spec,
};
