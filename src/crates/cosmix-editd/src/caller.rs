//! Caller identity and origin derivation (plan §4.3, D12).
//!
//! noded strips client `broker_*`/`mesh_from` headers
//! (`cosmix-noded/src/subscription.rs` `RESERVED_HEADERS`), stamps
//! `broker_origin` from the source socket, sets `from` to the caller's
//! REGISTERED service name (removed for anonymous connections), and stamps
//! `broker_peer` + `broker_service` for admitted mesh callers.
//!
//! # Contract (frozen)
//! 1. A mutating command with no `broker_origin` → INVALID_ARGUMENT `unstamped`.
//! 2. [`CallerKey`] (holders + dedup, never a gate): local registered →
//!    `local:<from>`; mesh → `mesh:<broker_service>@<broker_peer>`; local
//!    anonymous → `anon`.
//! 3. Origin = the caller's claim (`origin` on mutating verbs; `as` on
//!    undo/redo, where `origin` names the lane), else derived `agent:<from>` /
//!    `agent:<service>@<peer>` / `agent:anon`. A claim that fails the label
//!    grammar → INVALID_ARGUMENT `bad_origin`. Derived labels over 64 chars →
//!    first 55 chars + `+` + 8 lowercase hex of blake3(full label).
//! 4. Kind rule, identical for `origin` and `as`: `human:` only for a local
//!    registered caller; `tool:` never; otherwise the label is kept with kind
//!    `agent` and the reply says `origin_downgraded: true`.
//! 5. `COSMIX_MESH_OPEN=0` (read once at start): mutating verbs from
//!    `broker_origin != local` → FORBIDDEN `mesh_locked`. Reads always allowed.
//!    This is the only authorization-shaped check in editd, and it is opt-in.

use cosmix_client::IncomingCommand;
use cosmix_edit_core::origin::{Origin, Via};
use cosmix_edit_core::wire::Refusal;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CallerKey {
    Local(String),
    Mesh { service: String, peer: String },
    Anon,
}

impl std::fmt::Display for CallerKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallerKey::Local(from) => write!(f, "local:{from}"),
            CallerKey::Mesh { service, peer } => write!(f, "mesh:{service}@{peer}"),
            CallerKey::Anon => f.write_str("anon"),
        }
    }
}

/// The resolved caller of one command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    pub key: CallerKey,
    pub origin: Origin,
    pub origin_downgraded: bool,
    pub via: Via,
}

/// Resolve the caller of `cmd` (contract above). `claim` is the `origin`
/// (or, for undo/redo, `as`) argument; `mutating` selects rules 1 and 5.
pub fn resolve(cmd: &IncomingCommand, claim: Option<&str>, mutating: bool, mesh_open: bool) -> Result<Caller, Refusal> {
    let _ = (cmd, claim, mutating, mesh_open);
    todo!("E0b: plan §4.3")
}
