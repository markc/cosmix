//! `ID` (RFC 2971) — client/server identification exchange.
//!
//! Phase 1 acceptance does not require parsing the client's `ID`
//! parameter list; we emit our own untagged `* ID` body and OK back.
//! The argument tail is logged at debug for matrix testing but not
//! validated (a malformed tail still gets OK — clients that send junk
//! here are common in the wild, and a BAD here would lock out
//! Thunderbird mid-handshake).

use crate::imap::response::{Status, tagged};

/// Build the response. `args` is the verbatim argument tail from the
/// codec; it is currently unused but accepted so future phases can
/// log or surface client identification.
pub fn handle(tag: &str, _args: &str) -> String {
    // Body order matches what Cyrus / Dovecot advertise so clients
    // that key off this field see a familiar layout.
    let body = "(\"name\" \"cosmix-maild\" \"version\" \"0.1.0\")";
    let mut out = format!("* ID {body}\r\n");
    out.push_str(&tagged(tag, Status::Ok, None, "ID completed"));
    out
}
