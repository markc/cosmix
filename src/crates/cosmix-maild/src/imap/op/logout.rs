//! `LOGOUT` — RFC 9051 §6.1.3.
//!
//! Returns `* BYE` then tagged `OK LOGOUT completed`. The caller
//! transitions to `State::Logout` and closes the connection after
//! flushing the response.

use crate::imap::response::{Status, tagged, untagged_bye};

pub fn handle(tag: &str) -> String {
    let mut out = untagged_bye("cosmix-maild IMAP closing");
    out.push_str(&tagged(tag, Status::Ok, None, "LOGOUT completed"));
    out
}
