//! The `ctk.*` desktop-surface grant vocabulary.
//!
//! ctkd's ephemeral surfaces are mesh-authorised with the same opaque capability
//! tokens as every other grant (see [`crate::caps`]); this module just pins their
//! **canonical spelling** so the ctkd bin, grant authors, and the
//! `cross_mesh_exposable` allowlist can never drift on a magic string.
//!
//! # notify.v1 posture (spec §5)
//!
//! - **[`CTK_NOTIFY`]** — the passive-notification capability.
//!   - *Local-node callers* hold it **by default**: the local broker connection is
//!     the trust boundary, exactly as for every existing verb. The ctkd bin grants
//!     it in its base [`cosmix_props::AuthPolicy`] for local transport identities;
//!     that policy is the bin's, not this crate's.
//!   - *Remote mesh peers* need an **explicit per-peer grant** carrying this token,
//!     resolved through [`crate::combinator::with_cross_mesh_grants`] against the
//!     `wgd.grants` + `cross_mesh_exposable` substrate. The remote surface is
//!     always origin-labelled, rate-limited, and urgency-clamped (`<= Normal`) by
//!     the broker core regardless of the grant.
//! - **[`CTK_DIALOG`]** — the *modal* capability, a **separate** grant that is
//!   **not** part of notify.v1. It stays gated on the interaction-service-broker
//!   B-2 authority layer; it is named here only so the two are never conflated.
//!
//! These are tokens, not policy: holding [`CTK_NOTIFY`] authorises *reaching* the
//! notify surface. It does not bypass the broker's origin labelling, dedupe, or
//! rate limits — those apply to authorised and local callers alike.
//!
//! # Application action posture
//!
//! [`CTK_ACTIONS`] grants remote invocation of registered application actions.
//! Local callers still rely on noded's canonical connection-derived `from`
//! identity and are admitted by default in the local trust domain. Remote
//! callers remain closed until noded can attach authenticated, non-wire-
//! assertable provenance and the app port can resolve this capability through
//! the normal cross-mesh grant intersection.

use crate::caps::{Cap, CapabilitySet};

/// Capability token authorising the passive `interact.notify` surface (notify.v1).
pub const CTK_NOTIFY: &str = "ctk.notify";

/// Capability token authorising remote `action.invoke` on a CTK app port.
pub const CTK_ACTIONS: &str = "ctk.actions";

/// Capability token authorising the *modal* `dialog.*` surface. **Not** part of
/// notify.v1 — gated on B-2; defined here only to reserve the spelling.
pub const CTK_DIALOG: &str = "ctk.dialog";

/// The [`CTK_NOTIFY`] token as a [`Cap`].
pub fn ctk_notify_cap() -> Cap {
    Cap::new(CTK_NOTIFY)
}

/// The [`CTK_ACTIONS`] token as a [`Cap`].
pub fn ctk_actions_cap() -> Cap {
    Cap::new(CTK_ACTIONS)
}

/// The capability bag a remote mesh peer must resolve to in order to reach the
/// notify surface — exactly `{ctk.notify}`. Use it as both the grant a trusting
/// mesh issues and the `cross_mesh_exposable` allowlist ctkd registers for the
/// notify namespace, so their intersection yields the token (see
/// [`crate::caps::resolve_cross_mesh_caps`]).
pub fn notify_v1_grant() -> CapabilitySet {
    [ctk_notify_cap()].into_iter().collect()
}

/// Whether a resolved capability set authorises the notify surface. Applied to
/// the [`CapabilitySet`] the [`crate::combinator`] produces for a caller.
pub fn grants_notify(caps: &CapabilitySet) -> bool {
    caps.contains(&ctk_notify_cap())
}

/// The exact grant/allowlist bag for remote application-action invocation.
pub fn actions_v1_grant() -> CapabilitySet {
    [ctk_actions_cap()].into_iter().collect()
}

/// Whether resolved caller capabilities authorise application actions.
pub fn grants_actions(caps: &CapabilitySet) -> bool {
    caps.contains(&ctk_actions_cap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_have_the_expected_canonical_spelling() {
        assert_eq!(CTK_NOTIFY, "ctk.notify");
        assert_eq!(CTK_ACTIONS, "ctk.actions");
        assert_eq!(CTK_DIALOG, "ctk.dialog");
        // notify and dialog are distinct grants — never conflated.
        assert_ne!(CTK_NOTIFY, CTK_DIALOG);
        assert_ne!(CTK_NOTIFY, CTK_ACTIONS);
    }

    #[test]
    fn notify_grant_holds_exactly_the_notify_token() {
        let g = notify_v1_grant();
        assert_eq!(g.len(), 1);
        assert!(grants_notify(&g));
    }

    #[test]
    fn a_dialog_only_grant_does_not_authorise_notify() {
        // A caller granted only the modal capability cannot reach notify, and
        // vice-versa — the surfaces gate independently.
        let dialog_only: CapabilitySet = [Cap::new(CTK_DIALOG)].into_iter().collect();
        assert!(!grants_notify(&dialog_only));
    }

    #[test]
    fn empty_grant_does_not_authorise_notify() {
        assert!(!grants_notify(&CapabilitySet::empty()));
        assert!(!grants_actions(&CapabilitySet::empty()));
    }

    #[test]
    fn action_grant_is_exact_and_independent() {
        let actions = actions_v1_grant();
        assert_eq!(actions.len(), 1);
        assert!(grants_actions(&actions));
        assert!(!grants_notify(&actions));
        assert!(!grants_actions(&notify_v1_grant()));
    }

    #[test]
    fn grant_and_exposable_intersect_to_the_token() {
        // The realistic path: a mesh's grant and ctkd's exposable allowlist are
        // both `{ctk.notify}`; their intersection (what the combinator keeps)
        // still authorises notify.
        let grant = notify_v1_grant();
        let exposable = notify_v1_grant();
        assert!(grants_notify(&grant.intersect(&exposable)));
    }
}
