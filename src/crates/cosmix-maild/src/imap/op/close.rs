//! `CLOSE` (RFC 9051 §6.4.2) / `UNSELECT` (RFC 3691 §2).
//!
//! Phase 2 T4: both verbs transition `Selected(_)` → `Authenticated`
//! and emit a tagged OK. They have identical semantics in Phase 2
//! because the `\Deleted` purge that distinguishes CLOSE from
//! UNSELECT requires STORE — which doesn't ship until Phase 4.
//!
//! The session loop performs the state transition; this module only
//! emits the wire response. The "must be in Selected" gate also
//! lives in the session loop so the BAD/NO response shape stays
//! uniform across verbs that share it (FETCH, SEARCH, … from Phase
//! 3+).
//!
//! The Phase 4 split, once \Deleted exists:
//! * CLOSE — purge \Deleted-flagged messages in the read-write
//!   selected mailbox (no EXPUNGE responses), then unselect.
//! * UNSELECT — unselect without any purge.

use crate::imap::response::{Status, tagged};

/// Build the CLOSE tagged response. The session loop calls this
/// after verifying `state.selected().is_some()` and is responsible
/// for transitioning the state back to `Authenticated`.
pub fn handle_close(tag: &str) -> String {
    tagged(tag, Status::Ok, None, "CLOSE completed")
}

/// Build the UNSELECT tagged response. Same shape as
/// [`handle_close`] in Phase 2.
pub fn handle_unselect(tag: &str) -> String {
    tagged(tag, Status::Ok, None, "UNSELECT completed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_response_shape() {
        assert_eq!(handle_close("A1"), "A1 OK CLOSE completed\r\n");
    }

    #[test]
    fn unselect_response_shape() {
        assert_eq!(handle_unselect("A1"), "A1 OK UNSELECT completed\r\n");
    }
}
