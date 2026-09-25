//! Load, save and disk identities (plan §4.7).
//!
//! # Contract: two identities per buffer (frozen)
//! - `base` — the disk content the buffer's `saved_rev` corresponds to; set at
//!   load, save and clean reload. A DIRTY buffer never advances `base` from an
//!   observation (an observation whose content hash equals `base` may refresh
//!   its inode/mtime).
//! - `observed` — the latest identity the watcher saw.
//!
//! # Contract: save (frozen)
//! The actor first fixes the destination precondition [`Expect`]:
//! plain save → `Identity(base)` if the buffer has one, else `Absent`;
//! save-as → reserve the destination with the router (`path_open` if taken),
//! then stat it: absent → `Absent`; present without `force` → release,
//! CONFLICT `exists`; present with `force` → `Identity(that stat, hashed)`.
//! Then: `expect_rev` check (scratch without path → `scratch_needs_path`);
//! write `<dir>/.<name>.editd-<pid>-<n>.tmp` (mode of the file replaced; new
//! files `0o666 & !umask`) and fsync; REVALIDATE the destination against
//! `Expect` immediately before replacement (mismatch → remove temp, release any
//! reservation, CONFLICT `disk_modified`; a plain save with `force` skips the
//! comparison); `rename`; fsync the directory (failure after a committed rename
//! → saved with `durable: false`); record `base`, `mark_saved`; save-as then
//! commits the rebind through the router.
//!
//! Honest guarantee: an atomic REPLACEMENT with a revalidated precondition,
//! not a filesystem compare-and-swap; a writer landing between the final
//! `stat` and the `rename` is overwritten.

use std::path::{Path, PathBuf};

use cosmix_edit_core::buffer::{Buffer, FileMeta};
use cosmix_edit_core::wire::Refusal;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskIdentity {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime_ns: i128,
    pub blake3: [u8; 32],
}

/// Save destination precondition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expect {
    Absent,
    Identity(DiskIdentity),
}

/// A loaded file.
pub struct Loaded {
    pub buffer: Buffer,
    pub meta: FileMeta,
    pub canonical: PathBuf,
    pub base: DiskIdentity,
}

/// Outcome of a committed save.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    pub base: DiskIdentity,
    pub file_bytes: usize,
    pub durable: bool,
    pub warning: Option<String>,
}

/// Resolve `~/` against `HOME`, require absolute, canonicalise (`bad_path`).
pub fn resolve_path(path: &str) -> Result<PathBuf, Refusal> {
    let _ = path;
    todo!("E0b")
}

/// Bounded load (stat size first; read at most `MAX_BUFFER_BYTES + 1`).
/// Blocking: call from `spawn_blocking`.
pub fn load(canonical: &Path) -> Result<Loaded, Refusal> {
    let _ = canonical;
    todo!("E0b")
}

/// Current identity of `path`, or `None` when absent. Blocking.
pub fn identity(path: &Path) -> std::io::Result<Option<DiskIdentity>> {
    let _ = path;
    todo!("E0b")
}

/// Steps 2-5 of the save contract. Blocking.
pub fn save(dest: &Path, bytes: &[u8], expect: Expect, force: bool) -> Result<Saved, Refusal> {
    let _ = (dest, bytes, expect, force);
    todo!("E0b")
}
