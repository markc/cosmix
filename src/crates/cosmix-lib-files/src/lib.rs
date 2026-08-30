//! `cosmix-lib-files` — the pure core of the files-as-truth markdown corpus
//! manager (the daemon is `cosmix-filesd`).
//!
//! Design: `_plan/2026-06-29-cosmix-corpus-manager.md`. This crate holds only
//! daemon-independent logic so it is fast and mesh-free to test
//! (`cargo test --no-default-features`) and reusable in-process by both the daemon
//! and a future desktop client:
//!
//! - [`frontmatter`] — canonical READ (via `cosmix_bus::bus::parse_lenient`, the
//!   one grammar) plus a **surgical, byte-preserving WRITE**. There is no lossless
//!   round-trip through the Bus parser and no header serializer anywhere, so edits
//!   are minimal in-place line replacements over the raw text — never a
//!   parse→serialize round trip (invariant #2/#6).
//! - [`fsops`] — the generic live-filesystem layer (the Dolphin-style file-manager
//!   backend): `read_dir`/`stat`/mkdir/copy/move/trash/delete over a *places*
//!   allowlist, with all path-scoping + per-verb `writable` enforcement. Distinct
//!   from the corpus index (`_plan/2026-06-30-dcs-dual-pane-file-manager.md`).
//! - [`atomic`] — write-temp-then-`rename(2)` so a crash never leaves a torn file.
//! - [`hash`] — BLAKE3 content hashing (the #6 verify key / #7 reconcile signal).
//! - [`id`] — UUIDv7 opaque identity (#4).
//! - [`links`] — `[[wikilink]]` / `[md](target)` extraction (#5/§4.5).
//! - [`schema`] — the per-corpus `index.db` SQL as constants (the daemon executes
//!   it; no rusqlite dependency here).
//! - [`diff`] — the pure reconcile diff (Added / Changed / Removed / Renamed /
//!   DuplicateId / Unmanaged) that drives §6's correctness pass.
//! - `store` (feature `sqlite`) — the rusqlite-backed index.db persistence: schema
//!   apply/migrate, the monotonic modseq allocator, the change stream, and
//!   upsert/tombstone/rename keyed by `rel_path`.
//! - `ingest` / `reconcile` (feature `sqlite`) — read a file into a `DocRecord`, and
//!   the reconcile pass (walk → diff → apply + conflict-file handling) that the
//!   daemon runs on a timer and after each watcher burst.

pub mod atomic;
pub mod diff;
pub mod error;
pub mod frontmatter;
pub mod fsops;
pub mod hash;
pub mod id;
#[cfg(feature = "sqlite")]
pub mod ingest;
pub mod links;
#[cfg(feature = "sqlite")]
pub mod reconcile;
pub mod schema;
#[cfg(feature = "sqlite")]
pub mod store;

pub use error::{FilesError, Result};
