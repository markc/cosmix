//! `NAMESPACE` — RFC 2342, also referenced by RFC 9051 capability
//! advertisement.
//!
//! v1 advertises a single personal namespace with prefix `""` and
//! hierarchy delimiter `"/"`. There are no Other-users or Shared
//! namespaces — Cosmix is per-account single-tenant today; if
//! shared mailboxes appear later the response shape grows here and
//! `mailbox_name.rs` grows decode rules to match.

use crate::imap::mailbox_name::HIERARCHY_DELIMITER;
use crate::imap::response::{Status, tagged};

/// Handle a `NAMESPACE` command in Auth state.
///
/// Emits:
/// ```text
/// * NAMESPACE (("" "/")) NIL NIL
/// <tag> OK NAMESPACE completed
/// ```
pub fn handle(tag: &str) -> String {
    let mut out = String::with_capacity(80);
    out.push_str(&format!(
        "* NAMESPACE ((\"\" \"{HIERARCHY_DELIMITER}\")) NIL NIL\r\n"
    ));
    out.push_str(&tagged(tag, Status::Ok, None, "NAMESPACE completed"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_response_has_personal_with_slash_delimiter() {
        let out = handle("a001");
        assert!(out.contains("* NAMESPACE ((\"\" \"/\")) NIL NIL\r\n"));
        assert!(out.contains("a001 OK NAMESPACE completed\r\n"));
    }

    #[test]
    fn namespace_response_ends_with_crlf() {
        let out = handle("x1");
        assert!(out.ends_with("\r\n"));
    }
}
