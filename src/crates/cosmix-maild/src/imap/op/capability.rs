//! `CAPABILITY` — legal in any state per RFC 9051.

use crate::imap::capabilities;
use crate::imap::config::ImapConfig;
use crate::imap::response::{Status, tagged, untagged_capability};

/// Returns the two response lines to emit: the untagged
/// `* CAPABILITY ...` line followed by the tagged `OK CAPABILITY
/// completed` line, concatenated.
pub fn handle(tag: &str, cfg: &ImapConfig) -> String {
    let body = match cfg.advertise_capabilities.as_ref() {
        Some(caps) => capabilities::render_owned(caps),
        None => capabilities::render(capabilities::INITIAL),
    };
    let mut out = untagged_capability(&body);
    out.push_str(&tagged(tag, Status::Ok, None, "CAPABILITY completed"));
    out
}
