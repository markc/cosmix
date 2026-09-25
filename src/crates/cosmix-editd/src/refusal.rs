//! Refusal rendering: `rc = 10`, body = `cosmix_edit_core::wire::Refusal`
//! (`{"error_code","message","reason","buffer","rev",…context}`).
//!
//! `cosmix-lib-bus` renders `{error_code, message}` as `"CODE: message"` for
//! Rust callers; Mix callers read `$reply.error_code` (broker route only —
//! editd opens no native Unix port).

use cosmix_edit_core::error::{CoreError, ErrorCode, reason};
use cosmix_edit_core::wire::Refusal;
use serde_json::Map;

use crate::limits::MAX_REFUSAL_BYTES;

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

/// `(rc, body)` for the Bus response, bounded by `MAX_REFUSAL_BYTES`: an
/// oversized refusal keeps its code, reason, rev and (a real-sized) buffer,
/// with the message shortened and any large context value dropped.
pub fn render(r: &Refusal) -> (u8, String) {
    // Serializing this plain struct cannot fail; "" would still be a refusal (rc 10).
    let body = serde_json::to_string(r).unwrap_or_default();
    if body.len() <= MAX_REFUSAL_BYTES {
        return (REFUSAL_RC, body);
    }
    let mut cut = r.clone();
    let mut end = MESSAGE_KEEP.min(cut.message.len());
    while !cut.message.is_char_boundary(end) {
        end -= 1;
    }
    cut.message.truncate(end);
    cut.message.push_str(" …(truncated)");
    cut.context.retain(|_, v| crate::events::encoded_len(v) <= CONTEXT_VALUE_KEEP);
    // A buffer id echoed from bad arguments can be any size; real ids are short.
    cut.buffer = cut.buffer.filter(|b| b.len() <= CONTEXT_VALUE_KEEP);
    (REFUSAL_RC, serde_json::to_string(&cut).unwrap_or_default())
}

/// What an oversized refusal keeps of its message, and of each context value.
const MESSAGE_KEEP: usize = 4096;
const CONTEXT_VALUE_KEEP: usize = 1024;

/// Builder sugar: name the buffer, the current rev, or an extra context field.
pub trait RefusalExt: Sized {
    fn buffer(self, buffer: &str) -> Self;
    fn rev(self, rev: u64) -> Self;
    fn with(self, key: &str, value: impl Into<serde_json::Value>) -> Self;
}

impl RefusalExt for Refusal {
    fn buffer(mut self, buffer: &str) -> Self {
        self.buffer = Some(buffer.to_string());
        self
    }

    fn rev(mut self, rev: u64) -> Self {
        self.rev = Some(rev);
        self
    }

    fn with(mut self, key: &str, value: impl Into<serde_json::Value>) -> Self {
        self.context.insert(key.to_string(), value.into());
        self
    }
}

/// INVALID_ARGUMENT `bad_args` (serde shape errors and the like).
pub fn bad_args(message: impl Into<String>) -> Refusal {
    refusal(ErrorCode::InvalidArgument, Some(reason::BAD_ARGS), message)
}

/// NOT_FOUND `unknown_buffer`.
pub fn unknown_buffer(buffer: &str) -> Refusal {
    refusal(ErrorCode::NotFound, Some(reason::UNKNOWN_BUFFER), format!("no buffer {buffer}")).buffer(buffer)
}

/// RESOURCE_LIMIT `busy` for a full actor inbox.
pub fn busy(buffer: &str, queued: usize) -> Refusal {
    refusal(
        ErrorCode::ResourceLimit,
        Some(reason::BUSY),
        format!("buffer {buffer} has {queued} queued commands; retry"),
    )
    .buffer(buffer)
}

/// RESOURCE_LIMIT `busy` for a full router inbox.
pub fn router_busy() -> Refusal {
    refusal(
        ErrorCode::ResourceLimit,
        Some(reason::BUSY),
        format!("editd has {} queued global commands; retry", crate::limits::ROUTER_INBOX),
    )
}

/// RESOURCE_LIMIT `budget`: the aggregate byte lease was refused.
pub fn budget(needed: u64) -> Refusal {
    refusal(
        ErrorCode::ResourceLimit,
        Some(reason::BUDGET),
        format!(
            "editd byte budget exhausted: {needed} more bytes do not fit in {}",
            crate::limits::MAX_TOTAL_BYTES
        ),
    )
}

/// INTERNAL: the owning task went away mid-request.
pub fn internal(message: impl Into<String>) -> Refusal {
    refusal(ErrorCode::Internal, None, message)
}

/// IO_ERROR with `errno` and `kind` context.
pub fn io_error(what: &str, error: &std::io::Error) -> Refusal {
    let mut r = refusal(ErrorCode::IoError, None, format!("{what}: {error}"))
        .with("kind", format!("{:?}", error.kind()));
    if let Some(errno) = error.raw_os_error() {
        r = r.with("errno", errno);
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_is_rc10_with_flattened_context() {
        let r = busy("b3_9f2c41a7", 256);
        let (rc, body) = render(&r);
        assert_eq!(rc, 10);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["error_code"], "RESOURCE_LIMIT");
        assert_eq!(v["reason"], "busy");
        assert_eq!(v["buffer"], "b3_9f2c41a7");
        assert!(v["rev"].is_null());

        let io = io_error("writing /x", &std::io::Error::from_raw_os_error(28));
        let v: serde_json::Value = serde_json::from_str(&render(&io).1).unwrap();
        assert_eq!(v["error_code"], "IO_ERROR");
        assert_eq!(v["errno"], 28);
        assert_eq!(v["kind"], "StorageFull");
    }

    #[test]
    fn oversized_refusals_are_cut_to_the_bound() {
        let r = refusal(ErrorCode::InvalidArgument, Some(reason::BAD_ARGS), "\u{1}".repeat(1 << 20))
            .buffer("b1_00000000")
            .rev(3)
            .with("big", "x".repeat(1 << 20))
            .with("small", 7);
        let (rc, body) = render(&r);
        assert_eq!(rc, REFUSAL_RC);
        assert!(body.len() <= MAX_REFUSAL_BYTES, "{} bytes", body.len());
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!((v["error_code"].as_str(), v["reason"].as_str()), (Some("INVALID_ARGUMENT"), Some("bad_args")));
        assert_eq!((v["buffer"].as_str(), v["rev"].as_u64()), (Some("b1_00000000"), Some(3)));
        assert!(v.get("big").is_none());
        assert_eq!(v["small"], 7);
    }

    #[test]
    fn from_core_lifts_rev_out_of_context() {
        let err = CoreError::new(ErrorCode::Conflict, reason::STALE_REV, "stale").with("rev", 43u64).with("x", 1);
        let r = from_core(err, Some("b1_00000000"));
        assert_eq!(r.rev, Some(43));
        assert_eq!(r.context.get("x"), Some(&serde_json::json!(1)));
        assert!(!r.context.contains_key("rev"));
    }
}
