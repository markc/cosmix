//! IMAP session state machine per RFC 9051 §3.
//!
//! `NotAuthenticated` → `Authenticated` → `Selected(SelectedMailbox)`
//! → back to `Authenticated` (CLOSE/UNSELECT) or `Logout` (from any).
//!
//! Phase 2 (T2) populates `Selected` with [`SelectedMailbox`] — the
//! per-connection snapshot of the chosen mailbox's IMAP-visible
//! identity (UIDVALIDITY / UIDNEXT / EXISTS / HIGHESTMODSEQ / r-o
//! flag). T2 lands the *type shape*; the SELECT/EXAMINE/UNSELECT/
//! CLOSE handlers that actually transition into and out of the
//! variant land in T4 alongside SELECT semantics. This split keeps
//! the enum churn separate from the protocol-behavior diff so each
//! review boundary is honest about what is being asserted.
//!
//! `State` is no longer `Copy` — [`SelectedMailbox`] carries the
//! mailbox name as an owned `String`. Equality checks should go
//! through the helper predicates (`is_logout()`,
//! `is_not_authenticated()`, `is_authenticated()`) rather than
//! `==`, since `State` deliberately does not derive `PartialEq`:
//! deep-equating a `SelectedMailbox` is never the semantically right
//! check at a state-gate site.

use cosmix_mds::ContainerId;

#[derive(Debug, Clone)]
pub enum State {
    NotAuthenticated,
    Authenticated,
    /// Mailbox is selected. Phase 2+ — the SELECT/EXAMINE handler
    /// constructs the inner snapshot from `MailboxRecord` at
    /// transition time. It captures the selected mailbox identity
    /// and the status values emitted at SELECT/EXAMINE time; later
    /// phases may maintain a separate current sequence/status view
    /// for FETCH/SEARCH/IDLE alongside this snapshot.
    Selected(SelectedMailbox),
    Logout,
}

/// Per-connection snapshot of the selected mailbox.
///
/// Captured at SELECT/EXAMINE transition time from
/// [`crate::mailstore::MailboxRecord`]. Field semantics map 1:1
/// onto the IMAP response codes the handler emits:
///
/// * `container_id` — internal mds id, never sent on the wire.
/// * `name` — the wire-visible mailbox name (composed from the mds
///   parent tree per the Phase 2 hierarchy decision: walk-and-
///   compose at SELECT time, no schema change).
/// * `uid_validity` → `OK [UIDVALIDITY n]`.
/// * `uid_next` → `OK [UIDNEXT n]`.
/// * `exists` → `* N EXISTS`.
/// * `highest_mod_seq` → `OK [HIGHESTMODSEQ n]` once CONDSTORE
///   is enabled (Phase 6).
/// * `read_only` distinguishes SELECT (`OK [READ-WRITE]`, false)
///   from EXAMINE (`OK [READ-ONLY]`, true). Mailbox-mutating
///   verbs against the selected mailbox (STORE, MOVE, EXPUNGE)
///   must reject with `NO [CANNOT]` when this is true. COPY is
///   *permitted* under EXAMINE (RFC 9051 §6.4.7 — the dest is
///   the mutating party); the IMAP layer threads `read_only`
///   into `MailStore::copy_message` so the junk-boundary
///   retrain enqueue is suppressed when the source is
///   read-only (no side-channel writes through an EXAMINE'd
///   mailbox).
#[derive(Debug, Clone)]
pub struct SelectedMailbox {
    pub container_id: ContainerId,
    pub name: String,
    pub uid_validity: u64,
    pub uid_next: u64,
    pub exists: u64,
    pub highest_mod_seq: u64,
    pub read_only: bool,
    /// UIDs (ascending) the client has been told about so far in
    /// this selection. Populated at SELECT/EXAMINE time from a
    /// substrate snapshot, then mutated in lockstep with every
    /// untagged `EXISTS`/`EXPUNGE` the session emits — EXPUNGE /
    /// UID EXPUNGE / MOVE-out remove UIDs; IDLE's recv loop both
    /// adds (ItemAdded → EXISTS) and removes (ItemRemoved →
    /// EXPUNGE). Drives the IDLE-entry catch-up diff so a client
    /// re-entering IDLE after an out-of-band mutation receives
    /// the EXPUNGEs / EXISTS it would otherwise never see.
    ///
    /// APPEND/COPY/MOVE-into-selected do **not** currently emit
    /// `EXISTS` to the calling session and therefore do not update
    /// this vector — IDLE entry compensates by diffing against a
    /// fresh substrate snapshot, surfacing additions as a single
    /// post-diff `EXISTS`.
    pub view_uids: Vec<u64>,
}

impl State {
    /// Is the connection in `Authenticated` or `Selected(_)`?
    pub fn is_authenticated(&self) -> bool {
        matches!(self, State::Authenticated | State::Selected(_))
    }

    /// Has the session entered `Logout`? Replaces direct `==`
    /// since `State` is no longer `PartialEq`.
    pub fn is_logout(&self) -> bool {
        matches!(self, State::Logout)
    }

    /// Is the connection still in `NotAuthenticated`? Used by
    /// LOGIN / AUTHENTICATE legality gates.
    pub fn is_not_authenticated(&self) -> bool {
        matches!(self, State::NotAuthenticated)
    }

    /// Borrow the [`SelectedMailbox`] payload when the session is
    /// in `Selected(_)`. Returns `None` otherwise. This is the
    /// canonical gate for verbs that require a selected mailbox
    /// (FETCH, SEARCH, STORE, COPY, MOVE, EXPUNGE, UNSELECT, CLOSE —
    /// Phase 4+), and for read-only checks against EXAMINE'd
    /// mailboxes.
    pub fn selected(&self) -> Option<&SelectedMailbox> {
        match self {
            State::Selected(s) => Some(s),
            _ => None,
        }
    }

    /// Mutable accessor for `Selected(_)`. Handlers that mutate
    /// the selected-session view (EXPUNGE / UID EXPUNGE / MOVE-out
    /// updating `view_uids`, IDLE updating it in lockstep with
    /// emitted untaggeds) use this to keep
    /// [`SelectedMailbox::view_uids`] in sync with what the client
    /// has actually been told.
    pub fn selected_mut(&mut self) -> Option<&mut SelectedMailbox> {
        match self {
            State::Selected(s) => Some(s),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_mds::ContainerId;
    use uuid::Uuid;

    fn sample_selected() -> SelectedMailbox {
        SelectedMailbox {
            container_id: ContainerId(Uuid::nil()),
            name: "INBOX".into(),
            uid_validity: 1,
            uid_next: 1,
            exists: 0,
            highest_mod_seq: 0,
            read_only: false,
            view_uids: Vec::new(),
        }
    }

    #[test]
    fn is_authenticated_covers_authenticated_and_selected() {
        assert!(!State::NotAuthenticated.is_authenticated());
        assert!(State::Authenticated.is_authenticated());
        assert!(State::Selected(sample_selected()).is_authenticated());
        assert!(!State::Logout.is_authenticated());
    }

    #[test]
    fn is_logout_matches_only_logout() {
        assert!(!State::NotAuthenticated.is_logout());
        assert!(!State::Authenticated.is_logout());
        assert!(!State::Selected(sample_selected()).is_logout());
        assert!(State::Logout.is_logout());
    }

    #[test]
    fn is_not_authenticated_matches_only_pre_auth() {
        assert!(State::NotAuthenticated.is_not_authenticated());
        assert!(!State::Authenticated.is_not_authenticated());
        assert!(!State::Selected(sample_selected()).is_not_authenticated());
        assert!(!State::Logout.is_not_authenticated());
    }

    #[test]
    fn selected_accessor_returns_payload_only_in_selected() {
        assert!(State::NotAuthenticated.selected().is_none());
        assert!(State::Authenticated.selected().is_none());
        assert!(State::Logout.selected().is_none());
        let sample = sample_selected();
        let sel = State::Selected(sample.clone());
        let got = sel.selected().expect("Selected(_) must expose payload");
        assert_eq!(got.name, sample.name);
        assert_eq!(got.container_id, sample.container_id);
        assert_eq!(got.uid_validity, sample.uid_validity);
        assert_eq!(got.read_only, sample.read_only);
    }

    #[test]
    fn selected_payload_carries_select_response_fields() {
        // Sanity: every field needed for the SELECT/EXAMINE
        // untagged-response block is reachable on the inner
        // payload. Guards against accidental field removal.
        let s = sample_selected();
        let _ = (
            s.container_id,
            s.name,
            s.uid_validity,
            s.uid_next,
            s.exists,
            s.highest_mod_seq,
            s.read_only,
        );
    }
}
