//! `CHECK` (RFC 3501 §6.4.1 / RFC 9051 §6.4.1).
//!
//! CHECK requests a checkpoint of the currently selected mailbox —
//! "any implementation-dependent housekeeping" the server wants to do
//! that is not visible to the client (e.g. flushing in-memory state to
//! disk). It is valid only in Selected state and takes no arguments.
//!
//! cosmix-maild has no deferred mailbox state to flush — writes land
//! synchronously through the mailstore — so a successful CHECK is
//! functionally identical to NOOP: it just emits a tagged OK. RFC 3501
//! explicitly permits this ("There is no guarantee that an EXPUNGE
//! response will be made; … CHECK can also be used as an alternative to
//! NOOP").
//!
//! The "must be in Selected" gate and the no-arguments check live in the
//! session loop so the BAD response shape stays uniform with the other
//! selected-state, zero-argument verbs (CLOSE / UNSELECT).

use crate::imap::response::{Status, tagged};

/// Build the CHECK tagged response. The session loop calls this only
/// after verifying `state.selected().is_some()` and that no arguments
/// were supplied.
pub fn handle(tag: &str) -> String {
    tagged(tag, Status::Ok, None, "CHECK completed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_response_shape() {
        assert_eq!(handle("A1"), "A1 OK CHECK completed\r\n");
    }
}
