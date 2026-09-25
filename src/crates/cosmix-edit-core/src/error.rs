//! Refusal vocabulary shared by the core and the `edit` citizen.
//!
//! Wire shape (decision 10): `rc = 10`, body
//! `{"error_code": CODE, "message": text, "reason"?: REASON, …context}`.
//! The core only ever produces `InvalidArgument`, `NotFound`, `Conflict`,
//! `ResourceLimit` and `Internal`; editd adds `IoError`, `UnknownVerb` and
//! `Forbidden` (the last only under the `COSMIX_MESH_OPEN=0` lock).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    InvalidArgument,
    NotFound,
    Conflict,
    ResourceLimit,
    IoError,
    UnknownVerb,
    Forbidden,
    Internal,
}

/// The frozen `reason` vocabulary. A refusal carries at most one reason; a
/// CONFLICT always carries one and the current `rev`.
pub mod reason {
    // INVALID_ARGUMENT
    pub const BAD_ARGS: &str = "bad_args";
    pub const NOT_CHAR_BOUNDARY: &str = "not_char_boundary";
    pub const LINE_OUT_OF_RANGE: &str = "line_out_of_range";
    pub const COL_OUT_OF_RANGE: &str = "col_out_of_range";
    pub const OFFSET_OUT_OF_RANGE: &str = "offset_out_of_range";
    pub const OVERLAP_IN_TXN: &str = "overlap_in_txn";
    pub const BASE_REV_NEEDS_OFFSETS: &str = "base_rev_needs_offsets";
    pub const BASE_REV_IN_FUTURE: &str = "base_rev_in_future";
    pub const BOTH_CAS: &str = "both_cas";
    pub const NOT_UTF8: &str = "not_utf8";
    pub const BAD_PATH: &str = "bad_path";
    pub const BAD_ORIGIN: &str = "bad_origin";
    pub const BAD_OP_ID: &str = "bad_op_id";
    pub const BAD_NAME: &str = "bad_name";
    pub const BAD_REGEX: &str = "bad_regex";
    pub const UNSTAMPED: &str = "unstamped";
    pub const SCRATCH_NEEDS_PATH: &str = "scratch_needs_path";
    // NOT_FOUND
    pub const UNKNOWN_BUFFER: &str = "unknown_buffer";
    pub const EPOCH_MISMATCH: &str = "epoch_mismatch";
    pub const FILE_NOT_FOUND: &str = "file_not_found";
    pub const UNKNOWN_ANCHOR: &str = "unknown_anchor";
    pub const NOTHING_TO_UNDO: &str = "nothing_to_undo";
    pub const NOTHING_TO_REDO: &str = "nothing_to_redo";
    pub const SNAPSHOT_EXPIRED: &str = "snapshot_expired";
    // CONFLICT
    pub const STALE_REV: &str = "stale_rev";
    pub const OVERLAP: &str = "overlap";
    pub const HISTORY_TRIMMED: &str = "history_trimmed";
    pub const UNDO_CONFLICT: &str = "undo_conflict";
    pub const DIRTY: &str = "dirty";
    pub const DISK_MODIFIED: &str = "disk_modified";
    pub const PATH_OPEN: &str = "path_open";
    pub const EXISTS: &str = "exists";
    // RESOURCE_LIMIT
    pub const TOO_LARGE: &str = "too_large";
    pub const TOO_MANY_LINES: &str = "too_many_lines";
    pub const TOO_MANY_BUFFERS: &str = "too_many_buffers";
    pub const BUDGET: &str = "budget";
    pub const OUT_OF_MEMORY: &str = "out_of_memory";
    pub const BUSY: &str = "busy";
    pub const LIMIT: &str = "limit";
    // FORBIDDEN
    pub const MESH_LOCKED: &str = "mesh_locked";
}

/// A core-level refusal. editd renders it verbatim into the wire body.
#[derive(Debug, Clone, PartialEq)]
pub struct CoreError {
    pub code: ErrorCode,
    pub reason: Option<&'static str>,
    pub message: String,
    /// Extra wire fields (`rev`, `buffer`, `intervening_rev`, …).
    pub context: Map<String, Value>,
}

impl CoreError {
    pub fn new(code: ErrorCode, reason: &'static str, message: impl Into<String>) -> Self {
        Self { code, reason: Some(reason), message: message.into(), context: Map::new() }
    }

    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.context.insert(key.to_string(), value.into());
        self
    }
}

impl std::fmt::Display for CoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for CoreError {}
