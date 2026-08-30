//! `maild.vtoken.*` Bus verbs — the vtoken registry's operator control surface
//! (plan `_plan/2026-06-21-vtoken-dispatch-implementation.md` Stage C). Replaces
//! the hand-SQL seeding: an operator can `add` / `list` / `rotate` / `disable` /
//! `lookup` tokens over Bus, and the Mix `vtoken` CLI re-points here.
//!
//! ## Auth
//!
//! EVERY verb is operator-gated — the Bus sender (`cmd.from`) must be in the
//! configured `vtoken_operators` allowlist (the same shape as
//! `retention_operators` gating `maild.retention.run`; empty allowlist ⇒
//! nobody; an anonymous caller, whose `from` noded strips to empty, is refused).
//! Token management mints/rotates the publish capability, so even the read
//! verbs (`list`/`lookup`) are operator-only — and they **redact `pin`/
//! `pin_prev`** so a secret never crosses the wire.

use std::sync::{Arc, Mutex};

use cosmix_client::IncomingCommand;
use rusqlite::Connection;
use serde_json::Value;

use crate::vtoken::{OpaqueRow, VtokenStore};

const RC_ERROR: u8 = 10;

/// Shared state for the `maild.vtoken.*` verbs.
#[derive(Clone)]
pub struct VtokenBusState {
    pub store: Arc<VtokenStore>,
    /// Operator allowlist (`cmd.from`) for the global CLI path (no envelope).
    pub operators: Arc<Vec<String>>,
    /// Delegated-peer allowlist (`cmd.from`) for the webd front-door path
    /// (a `$cosmix_delegation` envelope present). DISTINCT from `operators`.
    pub delegated_peers: Arc<Vec<String>>,
    /// Main maild DB connection — the delegated path validates the actor and
    /// target accounts exist (`db::account::get_by_email`).
    pub db: Arc<Mutex<Connection>>,
}

/// The top-level body key carrying webd's trusted delegation envelope. Its
/// PRESENCE selects the delegated path; the operator (CLI) path's body never
/// has it. maild reads the envelope ONLY from here — any same-named key inside
/// `args` is plain user data, ignored for auth.
const DELEGATION_KEY: &str = "$cosmix_delegation";
/// The only delegation envelope version this maild understands.
const ENVELOPE_VERSION: i64 = 1;

/// Dispatch a `maild.vtoken.*` command. Returns `(rc, body_json)`.
///
/// Two mutually-exclusive auth paths, selected by the body:
/// - body has a top-level `$cosmix_delegation` → the DELEGATED path (webd
///   relaying a vhost-admin actor); it NEVER falls back to operator auth.
/// - otherwise → the OPERATOR path (the trusted CLI), gated on `vtoken_operators`.
pub async fn dispatch(action: &str, cmd: &IncomingCommand, state: &VtokenBusState) -> (u8, String) {
    if let Some((envelope, args)) = detect_delegation(cmd) {
        return dispatch_delegated(action, cmd, &envelope, &args, state).await;
    }
    // Operator gate (all verbs): the Bus sender must be in the allowlist.
    let from = cmd.from.as_str();
    // Defence-in-depth against a config overlap: a DELEGATED peer must ALWAYS
    // present an envelope — it can NEVER fall through to the global operator
    // path (else a compromised/buggy webd that strips the envelope, while also
    // listed in `vtoken_operators`, would land on operator auth). A delegated
    // peer here (no envelope) is refused outright, independent of the operator
    // allowlist.
    if !from.is_empty() && state.delegated_peers.iter().any(|p| p == from) {
        tracing::warn!(
            peer = from,
            action = action,
            "vtoken: delegated peer used the operator path (no envelope); refused"
        );
        return (
            RC_ERROR,
            err_body(
                "auth_denied: a delegated peer must present a $cosmix_delegation envelope; the operator path is not available to it",
            ),
        );
    }
    if from.is_empty() || !state.operators.iter().any(|o| o == from) {
        return (
            RC_ERROR,
            err_body("auth_denied: maild.vtoken.* requires an Bus sender in vtoken_operators"),
        );
    }
    match action {
        // Opaque (C9) verbs — the sender-locked single-segment namespace.
        "mint_opaque" => handle_mint_opaque(cmd, state).await,
        "list_opaque" => handle_list_opaque(state).await,
        "lookup_opaque" => handle_lookup_opaque(cmd, state).await,
        "disable_opaque" => handle_disable_opaque(cmd, state).await,
        other => (
            RC_ERROR,
            err_body(&format!("unknown vtoken action: {other}")),
        ),
    }
}

// ─── Opaque (C9) operator handlers ───────────────────────────────────────────

/// `maild.vtoken.mint_opaque` — mint an OPAQUE, sender-locked, single-segment
/// token. Args: `account`, `real_email`, `verification_strength`
/// (`local-auth`|`external-dmarc`), `service`; optional `allowed_sender`
/// (default = `real_email`) and `active` (default true). The recipient domain is
/// the account's domain. The plaintext token + address are returned ONCE; only
/// the HMAC is stored.
async fn handle_mint_opaque(cmd: &IncomingCommand, state: &VtokenBusState) -> (u8, String) {
    let args = match super::try_resolve_args(cmd) {
        Ok(v) => v,
        Err(e) => return (RC_ERROR, err_body(&format!("invalid args: {e}"))),
    };
    let active = match opt_bool(&args, "active", true) {
        Ok(b) => b,
        Err(e) => return (RC_ERROR, err_body(&e)),
    };
    mint_opaque_common(&args, active, state).await
}

/// Shared opaque-mint core for the operator + delegated paths. Parses the mint
/// args, derives the recipient domain from the account, mints, and returns the
/// plaintext token + address ONCE. The CALLER does the authz (operator
/// allowlist, or the delegated domain-bind + account-exists on `account`).
async fn mint_opaque_common(args: &Value, active: bool, state: &VtokenBusState) -> (u8, String) {
    let account = match req_email(args, "account") {
        Ok(s) => s,
        Err(e) => return (RC_ERROR, err_body(&e)),
    };
    let real_email = match req_email(args, "real_email") {
        Ok(s) => s,
        Err(e) => return (RC_ERROR, err_body(&e)),
    };
    // allowed_sender defaults to real_email (the owner posts from their own
    // verified address); an explicit value sets a different sender-lock. The
    // address itself is validated inside `mint_opaque` (canonical_addr).
    let allowed_sender = match opt_str(args, "allowed_sender") {
        Ok(Some(s)) => s,
        Ok(None) => real_email.clone(),
        Err(e) => return (RC_ERROR, err_body(&e)),
    };
    let verification_strength = match req_str(args, "verification_strength") {
        Ok(s) => s,
        Err(e) => return (RC_ERROR, err_body(&e)),
    };
    let service = match req_str(args, "service") {
        Ok(s) => s,
        Err(e) => return (RC_ERROR, err_body(&e)),
    };
    // The recipient domain (where the `<token>@domain` address lives) is the
    // account's domain — `req_email` guaranteed exactly one '@'.
    let domain = account.rsplit_once('@').map(|(_, d)| d).unwrap_or("");
    match state
        .store
        .mint_opaque(
            domain,
            &account,
            &real_email,
            &allowed_sender,
            &verification_strength,
            &service,
            active,
        )
        .await
    {
        Ok(m) => (
            0,
            serde_json::json!({
                "token_hmac": m.token_hmac,
                "token": m.token,
                "address": format!("{}@{}", m.token, m.domain),
                "domain": m.domain,
                "account": account,
                "service": service,
                "allowed_sender": allowed_sender,
                "verification_strength": verification_strength,
                "active": active,
                "minted": true,
                "ok": true,
            })
            .to_string(),
        ),
        Err(e) => (RC_ERROR, err_body(&format!("mint_opaque failed: {e}"))),
    }
}

/// `maild.vtoken.list_opaque` — every opaque token row. No secret to redact (the
/// plaintext token is never stored, only its HMAC).
async fn handle_list_opaque(state: &VtokenBusState) -> (u8, String) {
    match state.store.list_opaque().await {
        Ok(rows) => {
            let out: Vec<Value> = rows.iter().map(opaque_row_json).collect();
            (
                0,
                serde_json::json!({ "tokens": out, "count": out.len() }).to_string(),
            )
        }
        Err(e) => (RC_ERROR, err_body(&format!("list_opaque failed: {e}"))),
    }
}

/// `maild.vtoken.lookup_opaque` — args `token_hmac` (the row id from list/mint).
async fn handle_lookup_opaque(cmd: &IncomingCommand, state: &VtokenBusState) -> (u8, String) {
    let args = match super::try_resolve_args(cmd) {
        Ok(v) => v,
        Err(e) => return (RC_ERROR, err_body(&format!("invalid args: {e}"))),
    };
    let token_hmac = match req_str(&args, "token_hmac") {
        Ok(s) => s,
        Err(e) => return (RC_ERROR, err_body(&e)),
    };
    match state.store.lookup_opaque_by_hmac(&token_hmac).await {
        Ok(Some(row)) => (
            0,
            serde_json::json!({ "token": opaque_row_json(&row) }).to_string(),
        ),
        Ok(None) => (
            RC_ERROR,
            err_body(&format!("unknown token_hmac: {token_hmac}")),
        ),
        Err(e) => (RC_ERROR, err_body(&format!("lookup_opaque failed: {e}"))),
    }
}

/// `maild.vtoken.disable_opaque` — args `token_hmac`. Revoke (active=0).
async fn handle_disable_opaque(cmd: &IncomingCommand, state: &VtokenBusState) -> (u8, String) {
    let args = match super::try_resolve_args(cmd) {
        Ok(v) => v,
        Err(e) => return (RC_ERROR, err_body(&format!("invalid args: {e}"))),
    };
    let token_hmac = match req_str(&args, "token_hmac") {
        Ok(s) => s,
        Err(e) => return (RC_ERROR, err_body(&e)),
    };
    match state.store.disable_opaque(&token_hmac).await {
        Ok(true) => (
            0,
            serde_json::json!({ "token_hmac": token_hmac, "disabled": true, "ok": true })
                .to_string(),
        ),
        Ok(false) => (
            RC_ERROR,
            err_body(&format!("unknown token_hmac: {token_hmac}")),
        ),
        Err(e) => (RC_ERROR, err_body(&format!("disable_opaque failed: {e}"))),
    }
}

// ─── Delegated path (vtoken C2) ──────────────────────────────────────────────
//
// webd relays a logged-in vhost-admin actor via the `$cosmix_delegation`
// envelope. webd is the policy decision point (admin RBAC + CSRF + a per-route
// exact-verb grant, all enforced in webd before the call); maild binds the
// actor to the target so a confused/compromised webd path can't manage tokens
// outside the actor's own domain. maild grows NO role model. Design:
// `~/.cosmix/_plan/2026-06-21-vtoken-c2-delegated-bus-design.md`.

/// The trusted fields webd injects (parsed only from the top-level
/// `$cosmix_delegation`). The Mix handler cannot write these — webd builds the
/// envelope from request state.
struct Delegation {
    actor: String,
    vhost: String,
    route_id: String,
    request_id: String,
}

/// Detect a delegated call: `Some((envelope, args))` iff the body is a JSON
/// object with a top-level `$cosmix_delegation` key. The inner `args` object
/// (the verb's own arguments) is returned separately — envelope and args are
/// NEVER merged, so a key like `actor` inside `args` is plain user data. A body
/// that is empty / not JSON / an object without the key → `None` (operator
/// path). A malformed envelope VALUE still selects the delegated path (its
/// presence is the discriminator); [`parse_envelope`] then rejects it, so a
/// delegated peer can never be silently demoted to the operator path.
fn detect_delegation(cmd: &IncomingCommand) -> Option<(Value, Value)> {
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
/// version, `role == "admin"`, `csrf_verified == true` (a real bool), and
/// non-empty `actor`/`vhost`/`route_id`/`request_id`. Any miss is an error —
/// the delegated path never proceeds on a half-formed envelope.
fn parse_envelope(env: &Value) -> Result<Delegation, String> {
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
        actor: req_env_str(obj, "actor")?,
        vhost: req_env_str(obj, "vhost")?,
        route_id: req_env_str(obj, "route_id")?,
        request_id: req_env_str(obj, "request_id")?,
    })
}

fn req_env_str(obj: &serde_json::Map<String, Value>, key: &str) -> Result<String, String> {
    match obj.get(key).and_then(Value::as_str) {
        Some(s) if !s.is_empty() => Ok(s.to_string()),
        _ => Err(format!("delegation envelope missing/empty field: {key}")),
    }
}

/// A valid lowercased hostname for the authz boundary: non-empty, ≤253 bytes,
/// dot-separated labels each 1..=63 bytes of `[a-z0-9-]` not starting/ending
/// with `-` (so no empty/leading/trailing/double-dot label). Single-label
/// hostnames are allowed (the bind only needs the three values parsed the SAME
/// strict way, then compared equal).
fn is_valid_hostname(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 253
        && h.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
}

/// The strict, lowercased domain of an email FOR THE AUTHZ BOUNDARY: exactly
/// one `@`, a non-empty local part, and a domain that is a valid hostname.
/// `None` on any violation — a last-line authz helper must not let a crafted
/// `a@b@c` / `a@` / `@x` / mixed-case / trailing-dot value masquerade as
/// same-domain. (Looser than RFC 5321; deliberately conservative.)
fn email_domain_strict(email: &str) -> Option<String> {
    let (local, domain) = email.split_once('@')?;
    if local.is_empty() || domain.contains('@') {
        return None; // empty local, or more than one '@'
    }
    let domain = domain.to_ascii_lowercase();
    is_valid_hostname(&domain).then_some(domain)
}

/// The actor↔target↔vhost domain bind (the maild-side authority limit):
/// `domain(actor) == domain(target_account) == hostname(vhost)`, all parsed the
/// SAME strict way. This is what stops a delegated path from minting/managing
/// tokens outside the authenticated admin's own domain.
fn check_domain_bind(actor: &str, target_account: &str, vhost: &str) -> Result<(), String> {
    let vhost = vhost.to_ascii_lowercase();
    if !is_valid_hostname(&vhost) {
        return Err("delegating vhost is not a valid hostname".to_string());
    }
    let ad = email_domain_strict(actor).ok_or("actor is not a valid email")?;
    if ad != vhost {
        return Err("actor domain does not match the delegating vhost".to_string());
    }
    let td = email_domain_strict(target_account).ok_or("target account is not a valid email")?;
    if td != vhost {
        return Err("target account is outside the delegating vhost's domain".to_string());
    }
    Ok(())
}

/// One audit line per delegated call outcome (Codex C2 §7).
fn audit_delegated(peer: &str, env: &Delegation, verb: &str, target: &str, rc: u8) {
    tracing::info!(
        peer = peer,
        actor = %env.actor,
        vhost = %env.vhost,
        route_id = %env.route_id,
        request_id = %env.request_id,
        verb = verb,
        target = target,
        rc = rc,
        "vtoken delegated call"
    );
}

/// Resolve the existence of an account by email. `Ok(true)` exists, `Ok(false)`
/// unknown, `Err` on a DB failure.
async fn account_exists(state: &VtokenBusState, email: &str) -> Result<bool, String> {
    crate::db::account::get_by_email(&state.db, email)
        .await
        .map(|a| a.is_some())
        .map_err(|e| format!("account lookup failed: {e}"))
}

/// The DELEGATED dispatcher. NEVER falls back to operator auth: a bad peer or
/// envelope is a hard refusal here.
async fn dispatch_delegated(
    action: &str,
    cmd: &IncomingCommand,
    envelope: &Value,
    args: &Value,
    state: &VtokenBusState,
) -> (u8, String) {
    let peer = cmd.from.as_str();
    // (1) Peer must be an allowlisted delegated peer (distinct from operators).
    if peer.is_empty() || !state.delegated_peers.iter().any(|p| p == peer) {
        tracing::warn!(
            peer = peer,
            action = action,
            "vtoken delegated: peer not allowlisted; refused"
        );
        return (
            RC_ERROR,
            err_body(
                "auth_denied: delegated maild.vtoken.* requires a peer in vtoken_delegated_peers",
            ),
        );
    }
    // (2) Envelope must be structurally valid (version/role/csrf/fields).
    let env = match parse_envelope(envelope) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(peer = peer, error = %e, "vtoken delegated: bad envelope; refused");
            return (
                RC_ERROR,
                err_body(&format!("invalid delegation envelope: {e}")),
            );
        }
    };
    // (3) The actor must be a real maild account.
    match account_exists(state, &env.actor).await {
        Ok(true) => {}
        Ok(false) => {
            audit_delegated(peer, &env, action, &env.actor, RC_ERROR);
            return (
                RC_ERROR,
                err_body("auth_denied: delegated actor is not a known account"),
            );
        }
        Err(e) => return (RC_ERROR, err_body(&e)),
    }
    // (4-7) Per-verb: resolve the target, bind it to the actor's domain, act,
    // audit. list is domain-scoped; mutating verbs bind on the token's account.
    match action {
        // Opaque (C9) verbs — domain-bound on the token's content account.
        "mint_opaque" => delegated_mint_opaque(peer, &env, args, state).await,
        "list_opaque" => delegated_list_opaque(peer, &env, state).await,
        "lookup_opaque" => delegated_lookup_opaque(peer, &env, args, state).await,
        "disable_opaque" => delegated_disable_opaque(peer, &env, args, state).await,
        other => (
            RC_ERROR,
            err_body(&format!("unknown vtoken action: {other}")),
        ),
    }
}

// ─── Opaque (C9) delegated handlers ──────────────────────────────────────────

/// `mint_opaque` (delegated) — MINT a sender-locked opaque token for
/// `args.account` (always active; the explicit/replace path stays operator-only).
/// Domain-bound to the actor/vhost on the content account; the account must exist.
async fn delegated_mint_opaque(
    peer: &str,
    env: &Delegation,
    args: &Value,
    state: &VtokenBusState,
) -> (u8, String) {
    let account = match req_email(args, "account") {
        Ok(s) => s,
        Err(e) => return (RC_ERROR, err_body(&e)),
    };
    if let Err(e) = check_domain_bind(&env.actor, &account, &env.vhost) {
        audit_delegated(peer, env, "mint_opaque", &account, RC_ERROR);
        return (RC_ERROR, err_body(&format!("auth_denied: {e}")));
    }
    match account_exists(state, &account).await {
        Ok(true) => {}
        Ok(false) => {
            audit_delegated(peer, env, "mint_opaque", &account, RC_ERROR);
            return (
                RC_ERROR,
                err_body("auth_denied: target content account does not exist"),
            );
        }
        Err(e) => return (RC_ERROR, err_body(&e)),
    }
    // Delegated mints are ALWAYS active — "mint disabled" is not a web surface
    // (disable afterwards), so the reply never hands back a dead address.
    let (rc, body) = mint_opaque_common(args, true, state).await;
    audit_delegated(peer, env, "mint_opaque", &account, rc);
    (rc, body)
}

/// `list_opaque` (delegated) — DOMAIN-SCOPED to the actor/vhost domain (only
/// tokens whose content account is in that domain). Never the operator-global list.
async fn delegated_list_opaque(
    peer: &str,
    env: &Delegation,
    state: &VtokenBusState,
) -> (u8, String) {
    let vhost = env.vhost.to_ascii_lowercase();
    if !is_valid_hostname(&vhost)
        || email_domain_strict(&env.actor).as_deref() != Some(vhost.as_str())
    {
        audit_delegated(peer, env, "list_opaque", &vhost, RC_ERROR);
        return (
            RC_ERROR,
            err_body("auth_denied: actor domain does not match the delegating vhost"),
        );
    }
    match state.store.list_opaque().await {
        Ok(rows) => {
            let scoped: Vec<Value> = rows
                .iter()
                .filter(|r| email_domain_strict(&r.account).as_deref() == Some(vhost.as_str()))
                .map(opaque_row_json)
                .collect();
            audit_delegated(peer, env, "list_opaque", &vhost, 0);
            (
                0,
                serde_json::json!({ "tokens": scoped, "count": scoped.len() }).to_string(),
            )
        }
        Err(e) => (RC_ERROR, err_body(&format!("list_opaque failed: {e}"))),
    }
}

/// `lookup_opaque` (delegated) — args `token_hmac`; bound to the actor's domain
/// via the token's content account.
async fn delegated_lookup_opaque(
    peer: &str,
    env: &Delegation,
    args: &Value,
    state: &VtokenBusState,
) -> (u8, String) {
    let token_hmac = match req_str(args, "token_hmac") {
        Ok(s) => s,
        Err(e) => return (RC_ERROR, err_body(&e)),
    };
    let row = match resolve_opaque_target_in_domain(peer, env, "lookup_opaque", &token_hmac, state)
        .await
    {
        Ok(r) => r,
        Err((rc, body)) => return (rc, body),
    };
    audit_delegated(peer, env, "lookup_opaque", &row.account, 0);
    (
        0,
        serde_json::json!({ "token": opaque_row_json(&row) }).to_string(),
    )
}

/// `disable_opaque` (delegated) — args `token_hmac`; revoke. Domain-bound via the
/// token's account, mutated CONDITIONALLY on that account (TOCTOU-safe).
async fn delegated_disable_opaque(
    peer: &str,
    env: &Delegation,
    args: &Value,
    state: &VtokenBusState,
) -> (u8, String) {
    let token_hmac = match req_str(args, "token_hmac") {
        Ok(s) => s,
        Err(e) => return (RC_ERROR, err_body(&e)),
    };
    let row = match resolve_opaque_target_in_domain(peer, env, "disable_opaque", &token_hmac, state)
        .await
    {
        Ok(r) => r,
        Err((rc, body)) => return (rc, body),
    };
    match state
        .store
        .disable_opaque_if_account(&token_hmac, &row.account)
        .await
    {
        Ok(true) => {
            audit_delegated(peer, env, "disable_opaque", &row.account, 0);
            (
                0,
                serde_json::json!({ "token_hmac": token_hmac, "disabled": true, "ok": true })
                    .to_string(),
            )
        }
        // The row's account changed (or it was deleted) between the bind and here.
        Ok(false) => {
            audit_delegated(peer, env, "disable_opaque", &row.account, RC_ERROR);
            (
                RC_ERROR,
                err_body("conflict: token changed during the request; retry"),
            )
        }
        Err(e) => (RC_ERROR, err_body(&format!("disable_opaque failed: {e}"))),
    }
}

/// Look up an opaque token by `token_hmac` and enforce the actor↔account↔vhost
/// domain bind. The opaque twin of [`resolve_target_in_domain`]; shared by the
/// delegated lookup/disable verbs so the bind is applied before any action.
async fn resolve_opaque_target_in_domain(
    peer: &str,
    env: &Delegation,
    verb: &str,
    token_hmac: &str,
    state: &VtokenBusState,
) -> Result<OpaqueRow, (u8, String)> {
    let row = match state.store.lookup_opaque_by_hmac(token_hmac).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            audit_delegated(peer, env, verb, token_hmac, RC_ERROR);
            return Err((
                RC_ERROR,
                err_body(&format!("unknown token_hmac: {token_hmac}")),
            ));
        }
        Err(e) => return Err((RC_ERROR, err_body(&format!("lookup_opaque failed: {e}")))),
    };
    if let Err(e) = check_domain_bind(&env.actor, &row.account, &env.vhost) {
        audit_delegated(peer, env, verb, &row.account, RC_ERROR);
        return Err((RC_ERROR, err_body(&format!("auth_denied: {e}"))));
    }
    Ok(row)
}

/// An opaque token row as JSON for the browse surface (`list_opaque`/
/// `lookup_opaque`). No secret to redact — the plaintext token is never stored,
/// only its HMAC; `token_hmac` is the stable, one-way row id.
fn opaque_row_json(row: &OpaqueRow) -> Value {
    serde_json::json!({
        "token_hmac": row.token_hmac,
        "domain": row.domain,
        "account": row.account,
        "real_email": row.real_email,
        "allowed_sender": row.allowed_sender,
        "verification_strength": row.verification_strength,
        "token_len": row.token_len,
        "service": row.service,
        "active": row.active,
    })
}

/// Read an optional string arg, STRICTLY: absent/null → `None`; a present
/// non-empty string → `Some`; a present-but-EMPTY string or a present
/// non-string → an **error**. The strictness matters because `add`/`rotate`
/// switch mode on presence — treating `user_id=""` as "absent" would let
/// malformed explicit input silently fall through to the mint path and create
/// a live token. "Omit it entirely to mint" is the only way to mean absent.
fn opt_str(args: &Value, key: &str) -> Result<Option<String>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if !s.is_empty() => Ok(Some(s.clone())),
        Some(Value::String(_)) => Err(format!(
            "{key} must not be empty (omit it entirely to mint a token)"
        )),
        Some(_) => Err(format!("{key} must be a string")),
    }
}

/// Require a string with EXACTLY one `@` and a non-empty local + domain — used
/// for `account`/`real_email` so `build_addresses` derives a sane domain and an
/// operator typo (`a@b@c`, `@x`, `x@`) can't mint a token with a surprising or
/// empty domain.
fn req_email(args: &Value, key: &str) -> Result<String, String> {
    let s = req_str(args, key)?;
    match s.split_once('@') {
        Some((local, domain))
            if !local.is_empty() && !domain.is_empty() && !domain.contains('@') =>
        {
            Ok(s)
        }
        _ => Err(format!(
            "{key} must be a valid email address (one '@', non-empty parts)"
        )),
    }
}

/// Read an optional boolean field STRICTLY: absent/null → `default`; a real
/// bool → its value; present-but-any-other-type → an error (no silent coercion
/// for a safety-sensitive flag).
fn opt_bool(args: &Value, key: &str, default: bool) -> Result<bool, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(format!("{key} must be a boolean")),
    }
}

/// Require a non-empty string field from the args object.
fn req_str(args: &Value, key: &str) -> Result<String, String> {
    match args.get(key).and_then(Value::as_str) {
        Some(s) if !s.is_empty() => Ok(s.to_string()),
        _ => Err(format!("missing or empty required field: {key}")),
    }
}

fn err_body(msg: &str) -> String {
    serde_json::json!({ "error": msg }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vtoken::VtokenStore;
    use std::collections::BTreeMap;

    /// An in-memory `accounts` table seeded with `emails` (matching the columns
    /// `db::account::get_by_email` reads), wrapped like the live maild DB.
    fn accounts_db(emails: &[&str]) -> Arc<Mutex<Connection>> {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE accounts (
                id INTEGER PRIMARY KEY, email TEXT NOT NULL, password TEXT NOT NULL DEFAULT '',
                name TEXT, quota INTEGER NOT NULL DEFAULT 0, spam_enabled INTEGER, spam_threshold REAL
             );",
        )
        .unwrap();
        for (i, e) in emails.iter().enumerate() {
            conn.execute(
                "INSERT INTO accounts (id, email, password) VALUES (?1, ?2, '')",
                rusqlite::params![i as i64 + 1, e],
            )
            .unwrap();
        }
        Arc::new(Mutex::new(conn))
    }

    async fn state(operators: &[&str]) -> (VtokenBusState, tempfile::TempDir) {
        state_full(operators, &[], &[]).await
    }

    /// Full state with delegated peers + a seeded accounts DB for the delegated
    /// tests.
    async fn state_full(
        operators: &[&str],
        delegated_peers: &[&str],
        accounts: &[&str],
    ) -> (VtokenBusState, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = VtokenStore::open(tmp.path().join("v.db")).await.unwrap();
        let st = VtokenBusState {
            store: Arc::new(store),
            operators: Arc::new(operators.iter().map(|s| s.to_string()).collect()),
            delegated_peers: Arc::new(delegated_peers.iter().map(|s| s.to_string()).collect()),
            db: accounts_db(accounts),
        };
        (st, tmp)
    }

    /// A request from `from` with a JSON body (the `client.call` shape the Mix
    /// CLI produces). An empty body models a no-arg verb.
    fn cmd(from: &str, body: Value) -> IncomingCommand {
        IncomingCommand {
            from: from.to_string(),
            command: "maild.vtoken.test".to_string(),
            id: None,
            args: Value::Null,
            body: if body.is_null() {
                String::new()
            } else {
                body.to_string()
            },
            headers: BTreeMap::new(),
        }
    }

    // ── Delegated path (C2) ──────────────────────────────────────────────

    fn good_env(actor: &str, vhost: &str) -> Value {
        serde_json::json!({
            "version": 1, "actor": actor, "vhost": vhost, "route_id": "admin-vtokens",
            "role": "admin", "csrf_verified": true, "request_id": "req-1"
        })
    }

    fn deleg_cmd(from: &str, envelope: Value, args: Value) -> IncomingCommand {
        let body = serde_json::json!({ "$cosmix_delegation": envelope, "args": args });
        IncomingCommand {
            from: from.to_string(),
            command: "maild.vtoken.test".to_string(),
            id: None,
            args: Value::Null,
            body: body.to_string(),
            headers: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn delegated_peer_not_allowlisted_is_refused() {
        let (st, _t) = state_full(&["vtoken-cli"], &["webd"], &["admin@example.org"]).await;
        let (rc, out) = dispatch(
            "list_opaque",
            &deleg_cmd(
                "evil",
                good_env("admin@example.org", "example.org"),
                serde_json::json!({}),
            ),
            &st,
        )
        .await;
        assert_eq!(rc, RC_ERROR, "{out}");
        assert!(out.contains("vtoken_delegated_peers"), "{out}");
    }

    #[tokio::test]
    async fn delegated_peer_without_envelope_is_refused_not_demoted_to_operator() {
        let (st, _t) = state_full(&["webd"], &["webd"], &["admin@example.org"]).await;
        let (rc, out) = dispatch("list_opaque", &cmd("webd", Value::Null), &st).await;
        assert_eq!(rc, RC_ERROR);
        assert!(
            out.contains("must present a $cosmix_delegation envelope"),
            "{out}"
        );
        let (st2, _t2) = state_full(&["vtoken-cli"], &["webd"], &["admin@example.org"]).await;
        let (rc2, _) = dispatch("list_opaque", &cmd("vtoken-cli", Value::Null), &st2).await;
        assert_eq!(rc2, 0);
    }

    #[tokio::test]
    async fn delegated_bad_envelope_variants_are_refused() {
        let (st, _t) = state_full(&[], &["webd"], &["admin@example.org"]).await;
        let mut bad = Vec::new();
        let mut e = good_env("admin@example.org", "example.org");
        e["csrf_verified"] = serde_json::json!(false);
        bad.push(e);
        let mut e = good_env("admin@example.org", "example.org");
        e["role"] = serde_json::json!("user");
        bad.push(e);
        let mut e = good_env("admin@example.org", "example.org");
        e["version"] = serde_json::json!(2);
        bad.push(e);
        let mut e = good_env("admin@example.org", "example.org");
        e["actor"] = serde_json::json!("");
        bad.push(e);
        for env in bad {
            let (rc, out) = dispatch(
                "list_opaque",
                &deleg_cmd("webd", env, serde_json::json!({})),
                &st,
            )
            .await;
            assert_eq!(rc, RC_ERROR, "bad envelope should refuse: {out}");
        }
    }

    #[tokio::test]
    async fn delegated_unknown_actor_is_refused() {
        let (st, _t) = state_full(&[], &["webd"], &[]).await;
        let (rc, out) = dispatch(
            "list_opaque",
            &deleg_cmd(
                "webd",
                good_env("ghost@example.org", "example.org"),
                serde_json::json!({}),
            ),
            &st,
        )
        .await;
        assert_eq!(rc, RC_ERROR);
        assert!(out.contains("not a known account"), "{out}");
    }

    #[tokio::test]
    async fn delegated_ignores_actor_smuggled_in_args() {
        // A bogus actor/vhost inside `args` must NOT override the envelope.
        let (st, _t) = state_full(&[], &["webd"], &["admin@example.org", "blog@example.org"]).await;
        let args = serde_json::json!({
            "account": "blog@example.org", "real_email": "admin@example.org",
            "verification_strength": "local-auth", "service": "posts",
            "actor": "evil@other.org", "vhost": "other.org", "csrf_verified": false
        });
        let (rc, _out) = dispatch(
            "mint_opaque",
            &deleg_cmd("webd", good_env("admin@example.org", "example.org"), args),
            &st,
        )
        .await;
        assert_eq!(
            rc, 0,
            "the envelope governs; smuggled args fields are ignored"
        );
    }

    #[tokio::test]
    async fn delegated_strict_domain_blocks_crafted_actor() {
        let (st, _t) = state_full(
            &[],
            &["webd"],
            &["admin@example.org@evil.org", "blog@evil.org"],
        )
        .await;
        let args = serde_json::json!({
            "account": "blog@evil.org", "real_email": "x@evil.org",
            "verification_strength": "local-auth", "service": "posts"
        });
        let (rc, out) = dispatch(
            "mint_opaque",
            &deleg_cmd(
                "webd",
                good_env("admin@example.org@evil.org", "evil.org"),
                args,
            ),
            &st,
        )
        .await;
        assert_eq!(
            rc, RC_ERROR,
            "a multi-@ actor must not bind same-domain: {out}"
        );
    }

    #[tokio::test]
    async fn delegated_cross_domain_target_is_refused() {
        let (st, _t) = state_full(&[], &["webd"], &["admin@example.org", "blog@other.org"]).await;
        let args = serde_json::json!({
            "account": "blog@other.org", "real_email": "admin@example.org",
            "verification_strength": "local-auth", "service": "posts"
        });
        let (rc, out) = dispatch(
            "mint_opaque",
            &deleg_cmd("webd", good_env("admin@example.org", "example.org"), args),
            &st,
        )
        .await;
        assert_eq!(rc, RC_ERROR);
        assert!(out.contains("auth_denied"), "{out}");
    }

    // ── Opaque (C9) verbs ────────────────────────────────────────────────

    #[tokio::test]
    async fn operator_mint_opaque_roundtrip_list_lookup_disable() {
        let (st, _tmp) = state(&["op"]).await;
        let body = serde_json::json!({
            "account": "blog@x.org", "real_email": "m@y.org",
            "verification_strength": "external-dmarc", "service": "posts"
        });
        let (rc, out) = dispatch("mint_opaque", &cmd("op", body), &st).await;
        assert_eq!(rc, 0, "{out}");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["minted"], serde_json::json!(true));
        let token = v["token"].as_str().unwrap().to_string();
        let hmac = v["token_hmac"].as_str().unwrap().to_string();
        assert_eq!(token.len(), 10, "external-dmarc → L=10");
        // allowed_sender defaulted to real_email; address uses the account domain.
        assert_eq!(v["allowed_sender"], serde_json::json!("m@y.org"));
        assert_eq!(v["address"], serde_json::json!(format!("{token}@x.org")));

        // list_opaque shows the row and NEVER leaks the plaintext token.
        let (rc, lout) = dispatch("list_opaque", &cmd("op", Value::Null), &st).await;
        assert_eq!(rc, 0);
        assert!(
            !lout.contains(&token),
            "list_opaque leaked the plaintext token"
        );
        let lv: Value = serde_json::from_str(&lout).unwrap();
        assert_eq!(lv["count"], serde_json::json!(1));

        // lookup by token_hmac.
        let (rc, kout) = dispatch(
            "lookup_opaque",
            &cmd("op", serde_json::json!({"token_hmac": hmac})),
            &st,
        )
        .await;
        assert_eq!(rc, 0, "{kout}");
        let kv: Value = serde_json::from_str(&kout).unwrap();
        assert_eq!(kv["token"]["service"], serde_json::json!("posts"));
        assert_eq!(kv["token"]["active"], serde_json::json!(true));

        // disable → inactive on the next lookup.
        let (rc, _) = dispatch(
            "disable_opaque",
            &cmd("op", serde_json::json!({"token_hmac": hmac})),
            &st,
        )
        .await;
        assert_eq!(rc, 0);
        let (_, kout2) = dispatch(
            "lookup_opaque",
            &cmd("op", serde_json::json!({"token_hmac": hmac})),
            &st,
        )
        .await;
        let kv2: Value = serde_json::from_str(&kout2).unwrap();
        assert_eq!(kv2["token"]["active"], serde_json::json!(false));
    }

    #[tokio::test]
    async fn delegated_mint_opaque_in_domain_succeeds_cross_domain_refused() {
        let (st, _t) = state_full(
            &[],
            &["webd"],
            &["admin@example.org", "blog@example.org", "admin@other.org"],
        )
        .await;
        // In-domain mint (local-auth → L=6).
        let args = serde_json::json!({
            "account": "blog@example.org", "real_email": "admin@example.org",
            "verification_strength": "local-auth", "service": "posts"
        });
        let (rc, out) = dispatch(
            "mint_opaque",
            &deleg_cmd("webd", good_env("admin@example.org", "example.org"), args),
            &st,
        )
        .await;
        assert_eq!(rc, 0, "{out}");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["token"].as_str().unwrap().len(), 6, "local-auth → L=6");
        let hmac = v["token_hmac"].as_str().unwrap().to_string();

        // list_opaque is domain-scoped and shows it.
        let (rc, lout) = dispatch(
            "list_opaque",
            &deleg_cmd(
                "webd",
                good_env("admin@example.org", "example.org"),
                serde_json::json!({}),
            ),
            &st,
        )
        .await;
        assert_eq!(rc, 0);
        let lv: Value = serde_json::from_str(&lout).unwrap();
        assert_eq!(lv["count"], serde_json::json!(1));

        // An other.org admin cannot disable an example.org token (domain bind on
        // the token's content account).
        let (rc, out) = dispatch(
            "disable_opaque",
            &deleg_cmd(
                "webd",
                good_env("admin@other.org", "other.org"),
                serde_json::json!({"token_hmac": hmac}),
            ),
            &st,
        )
        .await;
        assert_eq!(rc, RC_ERROR, "{out}");
        assert!(out.contains("auth_denied"), "{out}");

        // Cross-domain mint target is refused too.
        let bad = serde_json::json!({
            "account": "blog@example.org", "real_email": "admin@other.org",
            "verification_strength": "local-auth", "service": "posts"
        });
        let (rc, out) = dispatch(
            "mint_opaque",
            &deleg_cmd("webd", good_env("admin@other.org", "other.org"), bad),
            &st,
        )
        .await;
        assert_eq!(rc, RC_ERROR, "{out}");
        assert!(out.contains("auth_denied"), "{out}");
    }
}
