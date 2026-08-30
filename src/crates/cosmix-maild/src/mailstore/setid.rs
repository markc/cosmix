//! `Account.id` ↔ `cosmix-mds::SetId` binding.
//!
//! We derive each account's MDS `SetId` deterministically from its
//! integer `account.id` PK using a fixed UUID-v5 namespace. This
//! avoids a schema migration to add an `accounts.mds_set_id` column
//! and avoids a per-call lookup; the price is an invariant on
//! `account.id` (see `feedback_account_setid_invariant.md`):
//!
//!   `account.id` MUST remain stable across export/import, account
//!   migration, and any future re-numbering. A change to `account.id`
//!   silently rebinds the account to a different MDS set, orphaning
//!   the prior on-disk MDS state (containers, items, blob_refs).
//!
//! If a future feature ever needs to re-issue account ids (tenant
//! transfer, account merge, …) the deterministic derivation below
//! must be replaced with a stored `accounts.mds_set_id` column and a
//! one-shot migration that captures the current binding before the
//! re-issuance.

use cosmix_mds::SetId;
use uuid::Uuid;

/// UUID-v5 namespace for `Account.id` → `SetId` derivation.
///
/// Stable forever — changing this constant remaps every account to a
/// new SetId and orphans all on-disk MDS state for existing accounts.
pub const MAILD_SET_NAMESPACE: Uuid = uuid::uuid!("6d61696c-6473-4e73-a1ac-636f736d6978");

/// Derive an account's `SetId` from its integer PK.
///
/// The hash input is the decimal string representation of `account_id`
/// (e.g. `b"1"`, `b"42"`). String form is used rather than the raw
/// `i32` little-endian bytes so the derivation matches the obvious
/// "what UUID does this account map to?" mental model and is trivial
/// to reproduce by hand from a database snapshot.
pub fn account_id_to_setid(account_id: i32) -> SetId {
    SetId(Uuid::new_v5(
        &MAILD_SET_NAMESPACE,
        account_id.to_string().as_bytes(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derivation_is_deterministic() {
        assert_eq!(account_id_to_setid(1), account_id_to_setid(1));
        assert_eq!(account_id_to_setid(42), account_id_to_setid(42));
    }

    #[test]
    fn distinct_accounts_get_distinct_sets() {
        assert_ne!(account_id_to_setid(1), account_id_to_setid(2));
        assert_ne!(account_id_to_setid(42), account_id_to_setid(43));
    }
}
