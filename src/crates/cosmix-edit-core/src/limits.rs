//! Core limits (ced E0 plan §3.8). Daemon policy limits live in
//! `cosmix-editd/src/limits.rs`.

const MIB: usize = 1024 * 1024;

/// Largest buffer text, checked at load and against every transaction's PEAK length.
pub const MAX_BUFFER_BYTES: usize = 64 * MIB;
/// Largest line count, checked at load and against every transaction's PEAK line count.
pub const MAX_LINES: usize = 2_000_000;
/// Inserted text per request.
pub const MAX_REQUEST_TEXT_BYTES: usize = MIB;
/// Ops per `edit.apply`.
pub const MAX_OPS_PER_TXN: usize = 10_000;
/// Retained log suffix: entries.
pub const LOG_MAX_ENTRIES: usize = 100_000;
/// Retained log suffix: stored inserted + deleted text bytes.
pub const LOG_MAX_TEXT_BYTES: usize = 32 * MIB;
/// Per-match text (and per-group text) in `find`.
pub const MATCH_TEXT_MAX: usize = 4 * 1024;
/// Per-match encoded size in `find` (text + groups + positions).
pub const MATCH_ENCODED_MAX: usize = 64 * 1024;
/// Changed spans listed in a mutation reply.
pub const REPLY_CHANGED_MAX: usize = 64;
/// Default and maximum `find` match counts.
pub const FIND_DEFAULT_LIMIT: usize = 1_000;
pub const FIND_MAX_LIMIT: usize = 10_000;
/// Regex compile size limit.
pub const REGEX_SIZE_LIMIT: usize = 10 * MIB;
/// Named anchors per buffer.
pub const MAX_ANCHORS: usize = 1_024;
/// Origins holding selections, and selections per origin.
pub const MAX_SELECTION_ORIGINS: usize = 64;
pub const MAX_SELECTIONS_PER_ORIGIN: usize = 16;
/// `history` entries whose edit text exceeds this are elided.
pub const HISTORY_ENTRY_TEXT_MAX: usize = 64 * 1024;
/// Origin label length.
pub const LABEL_MAX: usize = 64;
