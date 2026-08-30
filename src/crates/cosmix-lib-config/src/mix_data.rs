//! Strict-data Mix file loader for substrate-internal data files.
//!
//! Loads `.spec.mix` / `.conf.mix` / `.journal.mix` / `.verdict.mix` /
//! `.call.mix` / `.state.mix` files in strict-data mode — the literal
//! subset of Mix syntax that admits scalars, lists, and nested maps but
//! rejects every executable construct (variable references, function
//! calls, send/sh, interpolation, control flow, arithmetic).
//!
//! Returns the parsed `Value` tree directly. For *typed* deserialisation
//! into config structs, [`crate::store::load_service`] (per-service
//! config) and `crate::node` (node config) go through the serde bridge
//! in cosmix-lib-mix (`from_conf_mix_str` / `to_conf_mix_string`); this
//! module is the lower-level entry point for callers that want to walk
//! the raw `Value` tree themselves (substrate state files). See
//! `_doc/2026-05-04-substrate-mix-data-formats.md` and the config
//! migration `_doc/planned/2026-05-29-conf-mix-config-migration.md`.
//!
//! # Example
//!
//! ```no_run
//! use std::path::Path;
//! let value = cosmix_config::load_mix_data(Path::new("example.spec.mix")).unwrap();
//! ```

use std::path::Path;

pub use cosmix_mix::error::{MixError, MixResult};
pub use cosmix_mix::value::Value;

/// Parse a strict-data Mix source string and return its `Value` tree.
pub fn parse_mix_data(source: &str) -> MixResult<Value> {
    cosmix_mix::parse_data(source)
}

/// Read a `.*.mix` substrate-data file from disk and parse it in
/// strict-data mode. Returns `MixError::RuntimeError` if the file
/// cannot be read; otherwise propagates parser errors.
pub fn load_mix_data(path: &Path) -> MixResult<Value> {
    cosmix_mix::parse_data_file(path)
}
