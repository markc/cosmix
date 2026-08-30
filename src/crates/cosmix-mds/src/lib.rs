//! `cosmix-mds` — metadata store.
//!
//! Per-container-set SQLite metadata + content-addressable blob CAS.
//! Designed for `cosmix-maild` and any future consumer whose data
//! shape is *immutable content + multi-container mutable metadata +
//! ordered access within a container + change log*.
//!
//! See `_doc/2026-04-29-cosmix-mds-doc.md` (design),
//! `_doc/2026-04-29-cosmix-mds-spec.md` (contract), and
//! `_doc/2026-04-29-cosmix-mds-plan.md` (phasing).
//!
//! Phase 0: trait surface and types only; every method on the stub
//! implementations panics with `unimplemented!()`. Phase 1a starts the
//! real work.

pub mod error;
pub mod types;

pub mod blob;
pub mod blob_index;
pub mod bus;
pub mod container;
pub mod export;
pub mod gc;
pub mod import;
pub mod notifier;
pub mod rebuild;
pub mod schema;
pub mod store;
pub mod verify;

#[cfg(any(test, feature = "mem-store"))]
pub mod mem;

pub use crate::error::{Error, Result};
pub use crate::store::{Mds, SqliteCasMds, SqliteSetTx};
pub use crate::types::*;

#[cfg(any(test, feature = "mem-store"))]
pub use crate::mem::MemMds;
