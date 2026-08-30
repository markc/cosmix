//! S6a: the `$cosmix_delegation` delegated-authz spine for filesd's verbs.
//!
//! This is the daemon-side half of the webd `bus_call` seam's security
//! invariant: a verb is only "delegated-safe" once its daemon INTERPRETS the
//! `$cosmix_delegation` envelope instead of authorizing the call as a trusted
//! mesh peer. Without this, adding `filesd.*` to webd's seam would be a
//! confused-deputy hole (any WG peer's webd-shaped call treated as admin).
//!
//! Mirrors maild's `bus/vtoken.rs` reference EXACTLY (Codex 2026-06-29, thread
//! 019f12f5 — "match maild's peer identity check exactly"): a body with a
//! top-level `$cosmix_delegation` selects the DELEGATED path; its *presence*,
//! not its validity, is the discriminator, so a malformed envelope is REJECTED,
//! never silently demoted to the trusted on-node path. The delegated path never
//! falls back to trusted-peer auth (fail closed).
//!
//! Phase-1 LEAN scope: a valid admin delegation from an allowlisted webd peer
//! IS the authorization — corpora are operator-owned / single-tenant. The
//! per-actor/per-corpus SPEC-12 grants namespace stays deferred; the actor is
//! parsed + audited here so it's ready to bind when grants land. (maild's
//! actor↔target↔vhost domain bind has no filesd analogue yet — a corpus is not
//! domain-scoped — so it is intentionally omitted until grants exist.)

use cosmix_client::IncomingCommand;
use serde_json::Value;

pub const DELEGATION_KEY: &str = "$cosmix_delegation";
pub const ENVELOPE_VERSION: i64 = 1;

/// The trusted fields webd injects (parsed only from the top-level envelope —
/// a Mix handler can never write these; webd builds them from request state).
#[derive(Debug, Clone)]
pub struct Delegation {
    pub actor: String,
    pub vhost: String,
    pub route_id: String,
    pub request_id: String,
}

/// `Some((envelope, inner_args))` iff the body is a JSON object with a top-level
/// `$cosmix_delegation`. The envelope and the verb's own `args` are returned
/// separately and NEVER merged — an `actor` key inside `args` is plain user
/// data. A body that is empty / not JSON / an object without the key → `None`
/// (the bare on-node path). A malformed envelope VALUE still returns `Some`
/// (presence is the discriminator); [`parse_envelope`] then rejects it.
pub fn detect(cmd: &IncomingCommand) -> Option<(Value, Value)> {
    if cmd.body.is_empty() {
        return None;
    }
    let parsed: Value = serde_json::from_str(&cmd.body).ok()?;
    let obj = parsed.as_object()?;
    let envelope = obj.get(DELEGATION_KEY)?.clone();
    let args = obj
        .get("args")
        .cloned()
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
    Some((envelope, args))
}

/// Validate + extract the delegation envelope. Strict: object shape, exact
/// version, `role == "admin"`, `csrf_verified == true` (a real bool, always set
/// by webd), and non-empty `actor`/`vhost`/`route_id`/`request_id`. Any miss is
/// an error — the delegated path never proceeds on a half-formed envelope.
pub fn parse_envelope(env: &Value) -> Result<Delegation, String> {
    let obj = env
        .as_object()
        .ok_or("$cosmix_delegation must be a JSON object")?;
    match obj.get("version").and_then(Value::as_i64) {
        Some(v) if v == ENVELOPE_VERSION => {}
        Some(v) => return Err(format!("unsupported delegation envelope version {v}")),
        None => return Err("delegation envelope missing an integer version".to_string()),
    }
    if obj.get("role").and_then(Value::as_str) != Some("admin") {
        return Err("delegated role must be \"admin\"".to_string());
    }
    match obj.get("csrf_verified") {
        Some(Value::Bool(true)) => {}
        _ => return Err("csrf_verified must be the boolean true".to_string()),
    }
    Ok(Delegation {
        actor: req_str(obj, "actor")?,
        vhost: req_str(obj, "vhost")?,
        route_id: req_str(obj, "route_id")?,
        request_id: req_str(obj, "request_id")?,
    })
}

fn req_str(obj: &serde_json::Map<String, Value>, key: &str) -> Result<String, String> {
    match obj.get(key).and_then(Value::as_str) {
        Some(s) if !s.is_empty() => Ok(s.to_string()),
        _ => Err(format!("delegation envelope missing/empty field: {key}")),
    }
}

/// One audit line per delegated call outcome (eprintln → journald via the unit's
/// StandardError=journal; cosmix_log integration is deferred). `verb` is the full
/// dotted command (`filesd.list` / `fs.move`) so corpus and fs-mode both log truly.
pub fn audit(peer: &str, env: &Delegation, verb: &str, rc: u8) {
    eprintln!(
        "cosmix-filesd: delegated verb={verb} rc={rc} peer={peer} actor={} vhost={} route_id={} request_id={}",
        env.actor, env.vhost, env.route_id, env.request_id
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cmd_with(from: &str, body: &str) -> IncomingCommand {
        IncomingCommand {
            from: from.to_string(),
            command: "filesd.list".to_string(),
            id: None,
            args: Value::Null,
            body: body.to_string(),
            headers: std::collections::BTreeMap::new(),
        }
    }

    fn good_env() -> Value {
        json!({
            "version": 1, "actor": "admin@example.org", "vhost": "example.org",
            "route_id": "corpus-notes", "role": "admin", "csrf_verified": true,
            "request_id": "req-1"
        })
    }

    #[test]
    fn detect_bare_vs_delegated() {
        // Bare: no body, plain args body, or object without the key → None.
        assert!(detect(&cmd_with("op", "")).is_none());
        assert!(detect(&cmd_with("op", r#"{"limit":10}"#)).is_none());
        assert!(detect(&cmd_with("op", "not json")).is_none());
        // Delegated: top-level key present → Some, inner args split out.
        let body =
            json!({ "$cosmix_delegation": good_env(), "args": {"path": "n.md"} }).to_string();
        let (env, args) = detect(&cmd_with("webd", &body)).expect("delegated");
        assert_eq!(args["path"], "n.md");
        assert!(parse_envelope(&env).is_ok());
    }

    #[test]
    fn detect_malformed_envelope_still_selects_delegated() {
        // A present-but-garbage envelope must NOT demote to the bare path.
        let body = json!({ "$cosmix_delegation": "nope", "args": {} }).to_string();
        let (env, _) = detect(&cmd_with("webd", &body)).expect("present → delegated");
        assert!(parse_envelope(&env).is_err()); // then rejected
    }

    #[test]
    fn parse_envelope_strictness() {
        assert!(parse_envelope(&good_env()).is_ok());
        // wrong version
        let mut e = good_env();
        e["version"] = json!(2);
        assert!(parse_envelope(&e).is_err());
        // wrong role
        let mut e = good_env();
        e["role"] = json!("user");
        assert!(parse_envelope(&e).is_err());
        // csrf not the bool true
        let mut e = good_env();
        e["csrf_verified"] = json!("true");
        assert!(parse_envelope(&e).is_err());
        // empty actor
        let mut e = good_env();
        e["actor"] = json!("");
        assert!(parse_envelope(&e).is_err());
        // missing request_id
        let mut e = good_env();
        e.as_object_mut().unwrap().remove("request_id");
        assert!(parse_envelope(&e).is_err());
    }
}
