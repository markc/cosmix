//! SPEC 12 property substrate adoption for cosmix-maild.
//!
//! Each managed namespace gets its own submodule with a
//! `NamespaceSpec` + `SqliteTableMapping` + hooks. The Serve path
//! wires them into a single `PropsRouter` and dispatches
//! `maild.props.*` Bus requests through it.

pub mod account_overrides;
pub mod accounts;
pub mod aliases;
pub mod domains;
pub mod engine_config;
pub mod retention;
pub mod tls_identities;
