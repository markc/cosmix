//! Customer billing-portal authentication (Codex ruling A, 2026-07-08).
//!
//! Real WHMCS-style email+password login for billing customers, backed by the
//! vhost's `biz` SQLite (`customers.password_hash`, argon2id PHC) — separate
//! from the maild-account unified login. The customer's **external billing
//! email** is the login identity; no renta.net mailbox required.
//!
//! Split (Codex): Mix owns the forms, magic-link token generation/storage
//! (`customer_tokens`, sha256-at-rest), and the set/reset emails via the
//! invoice-mail path. This module owns ONLY the crypto-sensitive endpoints:
//!   * `POST /portal/login` — argon2-verify a password, seal a `kind="customer"`
//!     session (empty maild_token → no JMAP authority; role capped at rank-1).
//!   * `POST /portal/set-password` — redeem a single-use token, argon2-hash the
//!     new password, mark the token used, and auto-login — all in ONE DB
//!     transaction so a token can never be spent twice.
//!
//! Reuses the existing per-email failed-login lockout (namespaced `customer:`
//! so a portal attempt can't lock an admin/maild login for the same address),
//! CSRF double-submit, and chacha20poly1305 session sealing.

use std::sync::Arc;

use argon2::password_hash::{SaltString, rand_core::OsRng};
use argon2::{Algorithm, Argon2, Params, PasswordHash, PasswordHasher, PasswordVerifier, Version};
use axum::{
    Form,
    extract::{Extension, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;

use crate::{
    NodeState, VhostState, csrf_set_cookie, login_clear_failures, login_try_consume_attempt,
    redirect_with_cookies, safe_next, session, session_set_cookie,
};

/// Minimum portal password length (Codex: reasonable minimum, no composition
/// rules — WHMCS-style). 12 chars.
const MIN_PASSWORD_LEN: usize = 12;

const ARGON2_M_COST: u32 = 65_536;
const ARGON2_T_COST: u32 = 3;
const ARGON2_P_COST: u32 = 1;
/// Self-generated hashes use exactly this memory cost, so they always pass the
/// verification ceiling. Imported hashes above it fail before Argon2 allocates.
const MAX_VERIFY_M_COST: u32 = ARGON2_M_COST;

/// A precomputed argon2id PHC verified against on the no-credential login path
/// so a missing/empty-password account costs the same wall-clock as a real
/// verify — no "does this email have a portal password?" timing oracle. It was
/// generated with `argon2_hasher()` from a fixed public dummy password.
const DUMMY_PHC: &str = "$argon2id$v=19$m=65536,t=3,p=1$Ga7SDPqBwK2SOzyxdZ5ZiA$OuwXvEY7BNO2ibVhV4W0dgm971IbyDHv/d07smqArjg";

/// Bound request-time argon2 memory to 2 × 64 MiB. Waiters queue on the
/// semaphore; the permit moves into the blocking task so cancellation cannot
/// release it while argon2 is still running.
static ARGON2_SEMAPHORE: Semaphore = Semaphore::const_new(2);

/// Normalize a login identifier: trim + lowercase. Used for BOTH the DB lookup
/// (case-insensitive, WHMCS-style — emails are effectively case-insensitive)
/// and the lockout key, so case/whitespace variants can't fragment the failure
/// counter (Codex).
fn norm_email(s: &str) -> String {
    s.trim().to_lowercase()
}

/// argon2id parameters for a single-operator box (Codex): m=64 MiB, t=3, p=1.
fn argon2_hasher() -> Argon2<'static> {
    // 65536 KiB = 64 MiB. `Params::new` only fails on out-of-range values;
    // these are in range, so the unwrap is a constant-fold, not a runtime risk.
    let params = Params::new(ARGON2_M_COST, ARGON2_T_COST, ARGON2_P_COST, None)
        .expect("valid argon2 params");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

/// Hash a password to an argon2id PHC string with a fresh random salt.
fn hash_password(password: &str) -> Option<String> {
    let salt = SaltString::generate(&mut OsRng);
    argon2_hasher()
        .hash_password(password.as_bytes(), &salt)
        .ok()
        .map(|h| h.to_string())
}

/// Verify a password against a stored PHC string. False on any parse/verify
/// failure — never leaks *why*.
fn verify_password(phc: &str, password: &str) -> bool {
    match PasswordHash::new(phc) {
        // Verify with the params baked into the stored PHC hash (so an older
        // hash with different params still verifies), but cap imported hashes'
        // memory cost before Argon2 allocates. A missing m uses Argon2's safe
        // default; malformed params fail closed in the verifier.
        Ok(parsed) => {
            let m_cost = parsed
                .params
                .get("m")
                .map(|value| value.decimal())
                .transpose();
            match m_cost {
                Ok(Some(m_cost)) if m_cost > MAX_VERIFY_M_COST => {
                    tracing::warn!(
                        m_cost,
                        max_m_cost = MAX_VERIFY_M_COST,
                        "portal password hash exceeds argon2 verification memory limit"
                    );
                    // Match the missing-account path's bounded Argon2 cost
                    // before rejecting the imported hash. This recurses once:
                    // DUMMY_PHC is exactly at the ceiling, so it takes the
                    // normal verification branch. Always discard its result.
                    let _ = verify_password(DUMMY_PHC, password);
                    return false;
                }
                Ok(_) => {}
                Err(_) => return false,
            }
            Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok()
        }
        Err(_) => false,
    }
}

/// Run CPU- and memory-heavy argon2 work away from the Tokio worker threads.
async fn run_argon2<T, F>(operation: F) -> T
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let permit = ARGON2_SEMAPHORE
        .acquire()
        .await
        .expect("static argon2 semaphore is never closed");
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        operation()
    })
    .await
    .expect("argon2 blocking task panicked")
}

async fn hash_password_async(password: String) -> Option<String> {
    run_argon2(move || hash_password(&password)).await
}

async fn verify_password_async(phc: String, password: String) -> bool {
    run_argon2(move || verify_password(&phc, &password)).await
}

/// Lowercase hex sha256 — byte-identical to Mix `hash_sha256`, so a token
/// hashed in a Mix handler matches the lookup here.
fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Build the `/portal/login` URL with an optional error code + `next`. The Mix
/// login form (`h_portal.mix`) renders a banner from `?err` and threads `next`.
fn portal_login_location(err: Option<&str>, next: &str) -> String {
    let mut q = form_urlencoded::Serializer::new(String::new());
    if let Some(e) = err {
        q.append_pair("err", e);
    }
    if next != "/portal" && next != "/" {
        q.append_pair("next", next);
    }
    let qs = q.finish();
    if qs.is_empty() {
        "/portal/login".to_string()
    } else {
        format!("/portal/login?{qs}")
    }
}

/// `/portal/set-password?token=…` URL with an optional error. The token is
/// re-embedded so the retry POST carries it again.
fn portal_setpw_location(err: Option<&str>, token: &str) -> String {
    let mut q = form_urlencoded::Serializer::new(String::new());
    q.append_pair("token", token);
    if let Some(e) = err {
        q.append_pair("err", e);
    }
    format!("/portal/set-password?{}", q.finish())
}

/// Seal a `kind="customer"` session cookie for `(email, customer_id)` and
/// redirect to `next` (PRG). Empty maild_token (no JMAP), fresh CSRF.
fn seal_customer_and_redirect(
    node: &NodeState,
    vhost: &VhostState,
    email: &str,
    customer_id: i64,
    next: &str,
) -> Response {
    let now = session::now_secs();
    let new_csrf = session::new_csrf_token();
    let payload = session::SessionPayload {
        vhost: vhost.fqdn.clone(),
        maild_token: String::new(),
        email: email.to_string(),
        iat: now,
        exp: now + session::SESSION_TTL_SECS,
        csrf: new_csrf.clone(),
        // Customer portal sessions don't ride the maild session_epochs store
        // (that's keyed by maild account); 0 is the stable no-revocation value.
        epoch: 0,
        kind: "customer".to_string(),
        customer_id,
    };
    match node.session.seal(&payload) {
        Ok(sealed) => redirect_with_cookies(
            next,
            &[
                session_set_cookie(&sealed, session::SESSION_TTL_SECS),
                csrf_set_cookie(&new_csrf, session::SESSION_TTL_SECS),
            ],
        ),
        Err(e) => {
            tracing::error!(error = %e, "portal: customer session seal failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Pre-auth CSRF double-submit check (same contract as `/auth/login`): the
/// readable cookie must equal the submitted hidden field.
fn csrf_ok(headers: &HeaderMap, submitted: &str) -> bool {
    session::cookie_value(headers, session::CSRF_COOKIE)
        .as_deref()
        .map(|c| session::csrf_eq(c, submitted))
        .unwrap_or(false)
}

#[derive(Deserialize)]
pub(crate) struct PortalLoginForm {
    email: String,
    password: String,
    csrf: String,
    #[serde(default)]
    next: String,
}

/// `POST /portal/login` — verify email+password against `customers`, seal a
/// customer session. Fail-closed on a vhost with no `customers` table.
pub(crate) async fn portal_login_post(
    State(node): State<Arc<NodeState>>,
    Extension(vhost): Extension<Arc<VhostState>>,
    headers: HeaderMap,
    Form(form): Form<PortalLoginForm>,
) -> Response {
    let next = safe_next(&form.next);
    let next = if next == "/" {
        "/portal".to_string()
    } else {
        next
    };
    let fresh = || {
        let csrf = session::new_csrf_token();
        vec![csrf_set_cookie(&csrf, session::SESSION_TTL_SECS)]
    };

    if !csrf_ok(&headers, &form.csrf) {
        return redirect_with_cookies(&portal_login_location(Some("expired"), &next), &fresh());
    }
    let email = norm_email(&form.email);
    // Separate lockout namespace so a customer attempt can't lock an admin
    // login. Reserve this attempt atomically BEFORE credential lookup or the
    // Argon queue: at most MAX_FAILURES concurrent guesses can pass this point.
    // The submitted identity is reserved identically whether or not it exists.
    let lock_key = format!("customer:{email}");
    if !login_try_consume_attempt(&node, &lock_key).await {
        return redirect_with_cookies(&portal_login_location(Some("locked"), &next), &fresh());
    }

    // Read the credential (and id) for this email. Case-insensitive match
    // (WHMCS-style). A missing table / no row / empty hash all mean "no portal
    // password" → invalid, never an error page.
    let cred: Option<(i64, String)> = match vhost.db.as_ref() {
        Some(db_mtx) => {
            let db = db_mtx.lock().await;
            db.query_row(
                "SELECT id, password_hash FROM customers \
                 WHERE email = ?1 COLLATE NOCASE AND status != 'closed'",
                rusqlite::params![email],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            )
            .ok()
        }
        None => None,
    };

    // Always run one argon2 verify (real hash, or the dummy on no-cred) so the
    // wall-clock is the same whether or not the email has a portal password.
    let (has_credential, phc) = match &cred {
        Some((_, hash)) if !hash.is_empty() => (true, hash.clone()),
        _ => (false, DUMMY_PHC.to_owned()),
    };
    let verified = verify_password_async(phc, form.password).await && has_credential;
    if !verified {
        return redirect_with_cookies(&portal_login_location(Some("invalid"), &next), &fresh());
    }

    let customer_id = cred.expect("verified implies Some").0;
    login_clear_failures(&node, &lock_key).await;
    seal_customer_and_redirect(&node, &vhost, &email, customer_id, &next)
}

#[derive(Deserialize)]
pub(crate) struct PortalSetPwForm {
    token: String,
    password: String,
    password_confirm: String,
    csrf: String,
}

/// `POST /portal/set-password` — redeem a single-use set/reset token and set
/// the argon2 password, then auto-login. Token redemption + password write +
/// used-marking happen in ONE transaction so a token is spent exactly once.
pub(crate) async fn portal_set_password_post(
    State(node): State<Arc<NodeState>>,
    Extension(vhost): Extension<Arc<VhostState>>,
    headers: HeaderMap,
    Form(form): Form<PortalSetPwForm>,
) -> Response {
    if !csrf_ok(&headers, &form.csrf) {
        return redirect_with_cookies(&portal_setpw_location(Some("expired"), &form.token), &{
            let csrf = session::new_csrf_token();
            vec![csrf_set_cookie(&csrf, session::SESSION_TTL_SECS)]
        });
    }
    if form.password != form.password_confirm {
        return redirect_with_cookies(&portal_setpw_location(Some("mismatch"), &form.token), &[]);
    }
    if form.password.chars().count() < MIN_PASSWORD_LEN {
        return redirect_with_cookies(&portal_setpw_location(Some("short"), &form.token), &[]);
    }
    let Some(phc) = hash_password_async(form.password).await else {
        return redirect_with_cookies(&portal_setpw_location(Some("error"), &form.token), &[]);
    };
    let token_hash = sha256_hex(&form.token);

    // One transaction: verify token unused+unexpired → set password → mark used.
    let Some(db_mtx) = vhost.db.clone() else {
        return redirect_with_cookies(&portal_setpw_location(Some("invalid"), &form.token), &[]);
    };
    let outcome: Result<(i64, String), ()> = {
        let mut guard = db_mtx.lock().await;
        match guard.transaction() {
            Ok(tx) => {
                let row = tx.query_row(
                    "SELECT ct.customer_id, c.email FROM customer_tokens ct \
                     JOIN customers c ON c.id = ct.customer_id \
                     WHERE ct.token_sha256 = ?1 AND ct.used_at IS NULL \
                       AND ct.expires_at > datetime('now') AND c.status != 'closed'",
                    rusqlite::params![token_hash],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
                );
                match row {
                    Ok((cid, email)) => {
                        let a = tx.execute(
                            "UPDATE customers SET password_hash = ?1, \
                             password_set_at = datetime('now'), updated = datetime('now') \
                             WHERE id = ?2",
                            rusqlite::params![phc, cid],
                        );
                        // Defense-in-depth: the mark-used re-asserts unused +
                        // unexpired and must flip EXACTLY one row (it can't, in
                        // this tx, but a belt-and-braces guard against any future
                        // refactor that widens the SELECT).
                        let b = tx.execute(
                            "UPDATE customer_tokens SET used_at = datetime('now') \
                             WHERE token_sha256 = ?1 AND used_at IS NULL \
                               AND expires_at > datetime('now')",
                            rusqlite::params![token_hash],
                        );
                        if a.is_ok() && matches!(b, Ok(1)) && tx.commit().is_ok() {
                            Ok((cid, email))
                        } else {
                            Err(())
                        }
                    }
                    Err(_) => Err(()), // no matching/live token
                }
            }
            Err(_) => Err(()),
        }
    };

    match outcome {
        Ok((customer_id, email)) => {
            seal_customer_and_redirect(&node, &vhost, &email, customer_id, "/portal")
        }
        Err(()) => redirect_with_cookies(&portal_setpw_location(Some("invalid"), &form.token), &[]),
    }
}

#[derive(Deserialize)]
pub(crate) struct PortalChangePwForm {
    old_password: String,
    password: String,
    password_confirm: String,
    csrf: String,
}

/// `POST /portal/change-password` — a SIGNED-IN customer changes their own
/// password (old → new, WHMCS-style). Requires a live `kind="customer"`
/// session; verifies the old password before writing the new one.
pub(crate) async fn portal_change_password_post(
    State(node): State<Arc<NodeState>>,
    Extension(vhost): Extension<Arc<VhostState>>,
    headers: HeaderMap,
    Form(form): Form<PortalChangePwForm>,
) -> Response {
    // Must be a live customer session (the sealed CSRF is the authoritative
    // double-submit token for an authenticated request).
    let payload = match session::cookie_value(&headers, session::SESSION_COOKIE)
        .and_then(|c| node.session.unseal(&c, &vhost.fqdn, session::now_secs()))
    {
        Some(p) if p.kind == "customer" => p,
        _ => return redirect_with_cookies("/portal/login", &[]),
    };
    if !session::csrf_eq(&payload.csrf, &form.csrf) {
        return redirect_with_cookies("/portal/profile?pwerr=expired", &[]);
    }
    if form.password != form.password_confirm {
        return redirect_with_cookies("/portal/profile?pwerr=mismatch", &[]);
    }
    if form.password.chars().count() < MIN_PASSWORD_LEN {
        return redirect_with_cookies("/portal/profile?pwerr=short", &[]);
    }
    let Some(db_mtx) = vhost.db.clone() else {
        return redirect_with_cookies("/portal/profile?pwerr=error", &[]);
    };
    // Verify the current password, then write the new hash — under the lock.
    let cid = payload.customer_id;
    let current: Option<String> = {
        let db = db_mtx.lock().await;
        db.query_row(
            "SELECT password_hash FROM customers WHERE id = ?1 AND status != 'closed'",
            rusqlite::params![cid],
            |r| r.get::<_, String>(0),
        )
        .ok()
    };
    let ok = match current {
        Some(h) if !h.is_empty() => verify_password_async(h, form.old_password).await,
        _ => false,
    };
    if !ok {
        return redirect_with_cookies("/portal/profile?pwerr=wrong", &[]);
    }
    let Some(phc) = hash_password_async(form.password).await else {
        return redirect_with_cookies("/portal/profile?pwerr=error", &[]);
    };
    let wrote = {
        let db = db_mtx.lock().await;
        db.execute(
            "UPDATE customers SET password_hash = ?1, password_set_at = datetime('now'), \
             updated = datetime('now') WHERE id = ?2 AND status != 'closed'",
            rusqlite::params![phc, cid],
        )
    };
    if matches!(wrote, Ok(1)) {
        redirect_with_cookies("/portal/profile?pwok=1", &[])
    } else {
        redirect_with_cookies("/portal/profile?pwerr=error", &[])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dummy_phc_is_valid_and_performs_a_real_verify() {
        let parsed = PasswordHash::new(DUMMY_PHC).expect("dummy PHC must parse");
        assert!(
            Argon2::default()
                .verify_password(b"wrong password entirely", &parsed)
                .is_err()
        );
    }

    #[test]
    fn dummy_phc_params_match_self_generated_hashes() {
        let salt = SaltString::generate(&mut OsRng);
        let generated = argon2_hasher()
            .hash_password(b"params-parity-test", &salt)
            .expect("hash");
        let dummy = PasswordHash::new(DUMMY_PHC).expect("dummy PHC must parse");

        assert_eq!(generated.algorithm, dummy.algorithm);
        assert_eq!(generated.version, dummy.version);
        for name in ["m", "t", "p"] {
            assert_eq!(
                generated
                    .params
                    .get(name)
                    .expect("generated param")
                    .decimal()
                    .expect("generated decimal param"),
                dummy
                    .params
                    .get(name)
                    .expect("dummy param")
                    .decimal()
                    .expect("dummy decimal param"),
                "parameter {name} differs"
            );
        }
    }

    #[test]
    fn verify_rejects_over_limit_memory_cost_without_panicking() {
        let over_limit = DUMMY_PHC.replacen("m=65536", &format!("m={}", MAX_VERIFY_M_COST + 1), 1);
        assert!(!verify_password(&over_limit, "wrong password entirely"));
        // The equalizer succeeds internally for the fixed dummy password, but
        // the out-of-policy stored hash must still never be accepted.
        assert!(!verify_password(
            &over_limit,
            "timing-equalizer-not-a-real-secret"
        ));
    }

    #[test]
    fn hash_verify_roundtrip() {
        let phc = hash_password("correct horse battery staple").expect("hash");
        assert!(phc.starts_with("$argon2id$"), "PHC: {phc}");
        assert!(verify_password(&phc, "correct horse battery staple"));
        assert!(!verify_password(&phc, "wrong password entirely"));
        // a fresh hash of the same password differs (random salt)
        let phc2 = hash_password("correct horse battery staple").expect("hash");
        assert_ne!(phc, phc2);
        assert!(verify_password(&phc2, "correct horse battery staple"));
    }

    #[test]
    fn verify_rejects_garbage_phc() {
        assert!(!verify_password("", "x"));
        assert!(!verify_password("not-a-phc-string", "x"));
    }

    #[test]
    fn sha256_matches_mix() {
        // Mix `hash_sha256("hello")` — lowercase hex, must match byte-for-byte
        // so a token hashed in a Mix handler is found by the redemption lookup.
        assert_eq!(
            sha256_hex("hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn login_location_shapes() {
        assert_eq!(portal_login_location(None, "/portal"), "/portal/login");
        assert_eq!(
            portal_login_location(Some("invalid"), "/portal"),
            "/portal/login?err=invalid"
        );
        assert!(
            portal_login_location(None, "/portal/invoices").contains("next=%2Fportal%2Finvoices")
        );
    }
}
