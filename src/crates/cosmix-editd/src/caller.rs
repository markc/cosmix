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
use cosmix_edit_core::error::{ErrorCode, reason};
use cosmix_edit_core::origin::{Origin, OriginKind, Via};
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
///
/// Check order follows the router's refusal precedence: the claim's grammar
/// (argument shape) first, then the unstamped refusal, then the mesh lock.
pub fn resolve(cmd: &IncomingCommand, claim: Option<&str>, mutating: bool, mesh_open: bool) -> Result<Caller, Refusal> {
    let claimed = match claim {
        Some(text) => Some(text.parse::<Origin>().map_err(|e| crate::refusal::from_core(e, None))?),
        None => None,
    };
    let via = via_of(cmd);
    let stamped = matches!(via.broker_origin.as_str(), "local" | "mesh");
    if mutating && !stamped {
        return Err(crate::refusal::refusal(
            ErrorCode::InvalidArgument,
            Some(reason::UNSTAMPED),
            "mutations require a broker-stamped caller (broker_origin local or mesh)",
        ));
    }
    if mutating && !mesh_open && via.broker_origin != "local" {
        return Err(crate::refusal::refusal(
            ErrorCode::Forbidden,
            Some(reason::MESH_LOCKED),
            "edit mutations are node-local only (mesh access locked: COSMIX_MESH_OPEN=0)",
        ));
    }
    let key = key_of(&via);
    let local_registered = matches!(key, CallerKey::Local(_));
    let (origin, origin_downgraded) = match claimed {
        Some(origin) => downgrade(origin, local_registered),
        None => (Origin::new(OriginKind::Agent, derived_label(&key)), false),
    };
    Ok(Caller { key, origin, origin_downgraded, via })
}

/// Rule 4: `human:` only for a local registered caller, `tool:` never.
pub fn downgrade(origin: Origin, local_registered: bool) -> (Origin, bool) {
    match origin.kind {
        OriginKind::Agent => (origin, false),
        OriginKind::Human if local_registered => (origin, false),
        OriginKind::Human | OriginKind::Tool => (Origin::new(OriginKind::Agent, origin.label), true),
    }
}

/// The broker-attested headers (`from` is the registered name, absent for an
/// anonymous connection and for mesh callers).
pub fn via_of(cmd: &IncomingCommand) -> Via {
    let header = |name: &str| {
        cmd.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
            .filter(|value| !value.is_empty())
    };
    let from = Some(cmd.from.clone()).filter(|f| !f.is_empty());
    Via {
        from,
        broker_origin: header("broker_origin").unwrap_or_default(),
        broker_peer: header("broker_peer"),
        broker_service: header("broker_service"),
    }
}

/// Rule 2. An unstamped (or partially stamped) caller is `anon`.
pub fn key_of(via: &Via) -> CallerKey {
    match via.broker_origin.as_str() {
        "local" => match &via.from {
            Some(from) => CallerKey::Local(from.clone()),
            None => CallerKey::Anon,
        },
        "mesh" => CallerKey::Mesh {
            service: via.broker_service.clone().unwrap_or_else(|| "unknown".into()),
            peer: via.broker_peer.clone().unwrap_or_else(|| "unknown".into()),
        },
        _ => CallerKey::Anon,
    }
}

/// Rule 3's derived label: `<from>` / `<service>@<peer>` / `anon`, mapped
/// into the label grammar (never a `:`), truncated with a blake3 suffix
/// past 64 chars so it always parses.
pub fn derived_label(key: &CallerKey) -> String {
    let raw = match key {
        CallerKey::Local(from) => from.clone(),
        CallerKey::Mesh { service, peer } => format!("{service}@{peer}"),
        CallerKey::Anon => "anon".to_string(),
    };
    let clean: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '@' | '/' | '+' | '-') { c } else { '_' })
        .collect();
    let clean = if clean.is_empty() { "anon".to_string() } else { clean };
    let hash = blake3::hash(clean.as_bytes()).to_hex();
    Origin::truncate_label(&clean, &hash.as_str()[..8])
}

/// `COSMIX_MESH_OPEN`, read once at start: only `0` locks mutations to local callers.
pub fn mesh_open_from_env() -> bool {
    std::env::var("COSMIX_MESH_OPEN").map(|v| v.trim() != "0").unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn cmd(from: &str, headers: &[(&str, &str)]) -> IncomingCommand {
        IncomingCommand {
            from: from.into(),
            command: "edit.insert".into(),
            id: Some("1".into()),
            args: serde_json::Value::Null,
            body: String::new(),
            headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<BTreeMap<_, _>>(),
        }
    }

    fn code(r: &Refusal) -> (ErrorCode, Option<&str>) {
        (r.error_code, r.reason.as_deref())
    }

    #[test]
    fn unstamped_mutation_is_refused_but_reads_pass() {
        let c = cmd("x", &[]);
        let err = resolve(&c, None, true, true).unwrap_err();
        assert_eq!(code(&err), (ErrorCode::InvalidArgument, Some("unstamped")));
        let read = resolve(&c, None, false, true).unwrap();
        assert_eq!(read.key, CallerKey::Anon);
    }

    #[test]
    fn keys_and_derived_origins() {
        let local = resolve(&cmd("ced", &[("broker_origin", "local")]), None, true, true).unwrap();
        assert_eq!(local.key.to_string(), "local:ced");
        assert_eq!(local.origin.to_string(), "agent:ced");
        let mesh = resolve(
            &cmd("", &[("broker_origin", "mesh"), ("broker_service", "term"), ("broker_peer", "beta")]),
            None,
            true,
            true,
        )
        .unwrap();
        assert_eq!(mesh.key.to_string(), "mesh:term@beta");
        assert_eq!(mesh.origin.to_string(), "agent:term@beta");
        let anon = resolve(&cmd("", &[("broker_origin", "local")]), None, true, true).unwrap();
        assert_eq!(anon.key.to_string(), "anon");
        assert_eq!(anon.origin.to_string(), "agent:anon");
    }

    #[test]
    fn human_only_for_local_registered_and_tool_never() {
        let local = cmd("ced", &[("broker_origin", "local")]);
        let r = resolve(&local, Some("human:mark"), true, true).unwrap();
        assert_eq!((r.origin.to_string(), r.origin_downgraded), ("human:mark".into(), false));
        let r = resolve(&local, Some("tool:disk"), true, true).unwrap();
        assert_eq!((r.origin.to_string(), r.origin_downgraded), ("agent:disk".into(), true));
        let anon = cmd("", &[("broker_origin", "local")]);
        let r = resolve(&anon, Some("human:mark"), true, true).unwrap();
        assert_eq!((r.origin.to_string(), r.origin_downgraded), ("agent:mark".into(), true));
        let mesh = cmd("", &[("broker_origin", "mesh"), ("broker_service", "ced"), ("broker_peer", "beta")]);
        let r = resolve(&mesh, Some("human:mark"), true, true).unwrap();
        assert_eq!((r.origin.to_string(), r.origin_downgraded), ("agent:mark".into(), true));
        let r = resolve(&mesh, Some("agent:bot"), true, true).unwrap();
        assert_eq!((r.origin.to_string(), r.origin_downgraded), ("agent:bot".into(), false));
    }

    #[test]
    fn bad_claim_precedes_unstamped() {
        let err = resolve(&cmd("", &[]), Some("agent:has space"), true, true).unwrap_err();
        assert_eq!(code(&err), (ErrorCode::InvalidArgument, Some("bad_origin")));
        let err = resolve(&cmd("", &[]), Some("nokind"), true, true).unwrap_err();
        assert_eq!(code(&err), (ErrorCode::InvalidArgument, Some("bad_origin")));
    }

    #[test]
    fn mesh_lock_refuses_mesh_mutations_only() {
        let mesh = cmd("", &[("broker_origin", "mesh"), ("broker_service", "a"), ("broker_peer", "b")]);
        let err = resolve(&mesh, None, true, false).unwrap_err();
        assert_eq!(code(&err), (ErrorCode::Forbidden, Some("mesh_locked")));
        assert!(resolve(&mesh, None, false, false).is_ok());
        let local = cmd("x", &[("broker_origin", "local")]);
        assert!(resolve(&local, None, true, false).is_ok());
    }

    #[test]
    fn long_derived_label_truncates_and_parses() {
        let long = "s".repeat(50);
        let key = CallerKey::Mesh { service: long.clone(), peer: "p".repeat(40) };
        let label = derived_label(&key);
        assert_eq!(label.len(), 64);
        assert_eq!(&label[55..56], "+");
        assert!(label[56..].bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
        let origin: Origin = format!("agent:{label}").parse().unwrap();
        assert_eq!(origin.label, label);
        // Stable, and distinct for a different full label.
        assert_eq!(label, derived_label(&key));
        let other = CallerKey::Mesh { service: long, peer: "q".repeat(40) };
        assert_ne!(label, derived_label(&other));
    }

    #[test]
    fn derived_labels_stay_in_the_grammar() {
        let label = derived_label(&CallerKey::Local("weird name:x".into()));
        assert!(Origin::is_valid_label(&label), "{label}");
        assert!(!label.contains(':'));
    }
}
