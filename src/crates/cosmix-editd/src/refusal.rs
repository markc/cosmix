//! Refusal rendering: `rc = 10`, body = `cosmix_edit_core::wire::Refusal`
//! (`{"error_code","message","reason","buffer","rev",…context}`).
//!
//! `cosmix-lib-bus` renders `{error_code, message}` as `"CODE: message"` for
//! Rust callers; Mix callers read `$reply.error_code` (broker route only —
//! editd opens no native Unix port).

use cosmix_edit_core::error::{CoreError, ErrorCode};
use cosmix_edit_core::wire::Refusal;
use serde_json::{Map, Value};

/// Every refusal uses this rc.
pub const REFUSAL_RC: u8 = 10;

/// Build a refusal body.
pub fn refusal(code: ErrorCode, reason: Option<&str>, message: impl Into<String>) -> Refusal {
    Refusal {
        error_code: code,
        message: message.into(),
        reason: reason.map(str::to_string),
        buffer: None,
        rev: None,
        context: Map::new(),
    }
}

/// A core refusal, with the buffer id editd knows it for.
pub fn from_core(err: CoreError, buffer: Option<&str>) -> Refusal {
    let mut context = err.context;
    let rev = context.remove("rev").and_then(|v| v.as_u64());
    Refusal {
        error_code: err.code,
        message: err.message,
        reason: err.reason.map(str::to_string),
        buffer: buffer.map(str::to_string),
        rev,
        context,
    }
}

/// `(rc, body)` for the Bus response.
pub fn render(r: &Refusal) -> (u8, String) {
    let body = serde_json::to_value(r).unwrap_or_else(|_| Value::Null).to_string();
    (REFUSAL_RC, body)
}
