//! `cosmix-maild` library surface.
//!
//! The crate ships both a binary (`cosmix-maild`, see `src/main.rs`)
//! and a library that exposes its wiring to integration tests. The
//! library declares the same module tree the binary used to inline;
//! `main.rs` re-uses these modules via the `cosmix_maild` crate name
//! rather than `mod` declarations of its own.
//!
//! The `runtime` module exposes the production build sequence as
//! `build_runtime(&Config, RuntimeOpts)`. Integration tests use it to
//! stand up an in-process maild against ephemeral tempdir / random
//! ports without replicating the wiring; `main.rs::Command::Serve`
//! drives the same entry point.

pub mod auth;
pub mod bus;
pub mod config;
pub mod dav;
pub mod db;
pub mod dkim_rebuild;
pub mod imap;
pub mod jmap;
pub mod keyword;
pub mod maillog;
pub mod mailstore;
pub mod props;
pub mod rdns;
pub mod retention_worker;
pub mod rule_stats;
pub mod rule_stats_store;
pub mod runtime;
pub mod smtp;
pub mod tls;
pub mod vtoken;
pub mod vtoken_secret;
