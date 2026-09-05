//! `maild.rules.*` Bus action handlers.
//!
//! Three actions:
//! - `maild.rules.reload` — re-read the on-disk pack (no-op if
//!   `rules_pack_path` is unset, in which case the current
//!   `pack_version` is returned unchanged).
//! - `maild.rules.stats` — process-lifetime monotonic counters plus
//!   pack metadata.
//! - `maild.rules.explain` — debug surface that runs `RuleEngine::explain`
//!   against a caller-supplied envelope + raw RFC 5322 message. Phase 2
//!   only supports `mail_auth: null` and synthesizes an all-temperror
//!   `VerifyResult` (no DNS lookup) — full mail-auth round-trip is a
//!   later phase.
//!
//! Each handler returns `(rc, body_json)`:
//! - `rc = 0` on success — the body is the JSON-serialized response
//!   shape from `cosmix-maild-rules`.
//! - `rc >= 10` on caller error (malformed JSON, bad base64, bad IP) —
//!   the body is `{"error": "..."}`.
//!
//! Engine errors (e.g. an I/O failure inside `reload()`) also surface
//! as `rc = 10` to keep the wire contract uniform: callers see one
//! error sentinel regardless of which side of the boundary failed.

use std::net::IpAddr;
use std::sync::Arc;

use base64::Engine as _;
use cosmix_client::IncomingCommand;
use cosmix_maild_rules::{AccountId, AccountOverrides, DefaultRuleEngine, RuleContext, RuleEngine};
use cosmix_props::runtime::Runtime;

use crate::db;
use crate::props::account_overrides::{ResolvedOverrides, resolve_account_overrides_by_email};
use crate::rule_stats::RuleStats;
use crate::smtp::inbound::synthesize_temperror_verify;

/// Caller-error rc. Matches the unknown-action sentinel in `bus::mod` —
/// `NodedClient::call` only surfaces `rc >= 10` as an error.
const RC_ERROR: u8 = 10;

/// Dispatch a `maild.rules.*` command. Returns `(rc, body_json)`.
///
/// Action arguments are resolved via `super::resolve_args` so the
/// `args:` header takes precedence over the body — see that helper
/// for the full ordering rationale.
pub async fn dispatch(
    action: &str,
    cmd: &IncomingCommand,
    rule_engine: &Arc<DefaultRuleEngine>,
    rule_stats: &Arc<RuleStats>,
    hostname: &str,
    overrides_runtime: &Arc<Runtime>,
    db: &db::Db,
) -> (u8, String) {
    match action {
        "reload" => handle_reload(rule_engine).await,
        // Stats uses the strict resolver because its only valid arg
        // shapes (`{}` and `{"top_n": <u64>}`) are JSON objects; a
        // malformed `args:` header or body that the loose resolver
        // would silently degrade to `Null` is a caller error worth
        // surfacing as `rc=10`.
        "stats" => match super::try_resolve_args(cmd) {
            Ok(args) => handle_stats(rule_engine, rule_stats, &args).await,
            Err(e) => (RC_ERROR, err_body(&format!("malformed stats request: {e}"))),
        },
        "explain" => {
            let args = super::resolve_args(cmd);
            handle_explain(rule_engine, &args, hostname, overrides_runtime, db).await
        }
        other => (
            RC_ERROR,
            err_body(&format!("unknown rules action: {other}")),
        ),
    }
}

async fn handle_reload(engine: &DefaultRuleEngine) -> (u8, String) {
    match engine.reload().await {
        Ok(report) => match serde_json::to_string(&report) {
            Ok(body) => (0, body),
            Err(e) => (RC_ERROR, err_body(&format!("serialize ReloadReport: {e}"))),
        },
        Err(e) => (RC_ERROR, err_body(&format!("reload failed: {e}"))),
    }
}

/// Default `top_n` cap on the `per_rule` map embedded in
/// `maild.rules.stats`. Chosen to comfortably exceed any realistic
/// pack size (typical packs are O(10) rules; the on-disk store also
/// retains entries for removed rules, so the historical map can
/// drift larger). Callers that need more can pass `top_n` up to
/// `MAX_PER_RULE_TOP_N`; `rules_with_hits` always reflects the
/// untruncated total cardinality so the caller can detect when more
/// entries exist than were returned.
const DEFAULT_PER_RULE_TOP_N: usize = 256;

/// Hard ceiling on `top_n`. Bounds the wire payload and the final
/// returned per-rule map of a single `maild.rules.stats` reply —
/// the verb returns a single JSON string body, so an unbounded
/// result map would translate directly into an unbounded
/// allocation + send. 4096 entries at ~80 bytes per entry caps the
/// per-rule slice at well under 1 MiB.
///
/// This does NOT bound the total in-memory selection work: for
/// `top_n > 0`, `snapshot_top_n_rules` clones the full historical
/// per-rule map (O(N) transient allocation) and then partial-sorts
/// it (O(N log top_n) work) outside the mutex. That O(N) cost is
/// accepted pending persistence-layer retention work; the only
/// cheap-by-construction path is `top_n == 0`, which reads
/// cardinality only.
const MAX_PER_RULE_TOP_N: usize = 4096;

/// Request shape for `maild.rules.stats`. Both arms (`{}` and
/// `{ "top_n": <u64> }`) deserialize cleanly via the optional field.
/// Non-object request bodies (`[]`, `"bad"`, `42`, etc.) are
/// rejected at deserialization with rc=10, matching the validation
/// discipline of `handle_explain` and other maild Bus verbs.
#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct StatsRequest {
    #[serde(default)]
    top_n: Option<u64>,
}

async fn handle_stats(
    engine: &DefaultRuleEngine,
    stats: &RuleStats,
    args: &serde_json::Value,
) -> (u8, String) {
    let req: StatsRequest = match args {
        // `resolve_args` returns Null when neither the `args:`
        // header nor a body payload is present — that's the
        // zero-arg call the doc shows as `{}`, so treat it as the
        // default request.
        serde_json::Value::Null => StatsRequest::default(),
        serde_json::Value::Object(_) => {
            match serde_json::from_value::<StatsRequest>(args.clone()) {
                Ok(r) => r,
                Err(e) => {
                    return (RC_ERROR, err_body(&format!("malformed stats request: {e}")));
                }
            }
        }
        // Non-object request shapes (arrays, strings, numbers,
        // booleans). serde would silently accept an empty array as
        // "all optional fields defaulted" without this gate, so we
        // surface them explicitly to match the documented contract.
        _ => {
            return (
                RC_ERROR,
                err_body("malformed stats request: expected object"),
            );
        }
    };
    let top_n = req
        .top_n
        .map(|n| (n as usize).min(MAX_PER_RULE_TOP_N))
        .unwrap_or(DEFAULT_PER_RULE_TOP_N);

    // pack_version + rules_loaded are read under a single lock so a
    // concurrent reload cannot swap the pack between the two reads
    // and produce a mixed-pack snapshot.
    let (pack_version, rules_loaded) = engine.pack_metadata().await;

    // Per-rule counters (Phase 2 commit 5). The on-disk store can
    // outgrow the current pack — removed-from-pack rules persist
    // there so operators can audit hit history after a pack edit.
    //
    // `snapshot_top_n_rules` bounds the final response map to
    // `top_n` entries and bounds the per-rule-mutex hold time to a
    // single HashMap clone (no scan-under-the-lock), so the
    // classify hot-path's `record_rule_hit` is never blocked by
    // selection work. The transient O(N) clone and the O(N log n)
    // partial-sort happen after the lock is released. For
    // `top_n == 0` the call short-circuits to a cardinality-only
    // read under the lock — no clone, no allocation.
    //
    // Total work per call is still proportional to stored per-rule
    // cardinality N. Capping N (eviction / retention policy) is a
    // persistence-layer concern tracked separately, not addressed
    // by this commit.
    let (snap, total_with_hits) = stats.snapshot_top_n_rules(top_n);
    // JSON object iteration order is not part of the wire contract
    // — callers MUST treat `per_rule` as an unordered map. The
    // deterministic part is the *membership* of the top-N: which
    // rule_ids appear is fixed by hit count desc, tiebroken by
    // rule_id asc. (The local insertion-order sort below exists to
    // keep test snapshots stable; it is not promised to consumers.)
    let per_rule_obj: serde_json::Map<String, serde_json::Value> = {
        let mut entries: Vec<(&String, &u64)> = snap.per_rule.iter().collect();
        entries.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        entries
            .into_iter()
            .map(|(id, count)| (id.clone(), serde_json::Value::from(*count)))
            .collect()
    };
    let per_rule_truncated = per_rule_obj.len() < total_with_hits;

    let body = serde_json::json!({
        "pack_version":  pack_version,
        "rules_loaded":  rules_loaded,
        "shadow_mode":   engine.shadow_mode().await,
        "verdicts_total": snap.verdicts_total,
        "verdicts_by_kind": {
            "hard_accept": snap.hard_accept,
            "hard_junk":   snap.hard_junk,
            "continue":    snap.continue_count,
        },
        "per_rule": per_rule_obj,
        "rules_with_hits": total_with_hits,
        "per_rule_truncated": per_rule_truncated,
    });
    (0, body.to_string())
}

#[derive(serde::Deserialize)]
struct ExplainRequest {
    /// Account id is stringified to match the `<base>/<id>/...` on-disk
    /// layout. Accept either int or string on the wire.
    #[serde(default)]
    account_id: Option<serde_json::Value>,
    envelope_from: String,
    envelope_to: Vec<String>,
    peer_ip: String,
    message_b64: String,
    /// Explain the message as if it arrived on a SASL-authenticated
    /// submission session (`RuleContext::sender_authenticated`): the
    /// mail-auth hard-fail rules and the auth half of the structural
    /// composite are then disabled, exactly as in delivery. Default false
    /// (MX provenance). Echoed back in the response.
    #[serde(default)]
    sender_authenticated: bool,
    /// Reserved for a future pre-verified VerifyResult passthrough.
    /// Phase 2 ignores any non-null value and always synthesizes
    /// all-temperror — the spec says explain does no DNS lookup.
    #[serde(default)]
    mail_auth: Option<serde_json::Value>,
}

async fn handle_explain(
    engine: &DefaultRuleEngine,
    args: &serde_json::Value,
    hostname: &str,
    overrides_runtime: &Arc<Runtime>,
    db: &db::Db,
) -> (u8, String) {
    let req: ExplainRequest = match serde_json::from_value(args.clone()) {
        Ok(r) => r,
        Err(e) => {
            return (
                RC_ERROR,
                err_body(&format!("malformed explain request: {e}")),
            );
        }
    };

    if req.mail_auth.as_ref().is_some_and(|v| !v.is_null()) {
        return (
            RC_ERROR,
            err_body(
                "mail_auth: passing a pre-verified VerifyResult is not supported in Phase 2; pass null to synthesize all-temperror",
            ),
        );
    }

    let peer_ip: IpAddr = match req.peer_ip.parse() {
        Ok(ip) => ip,
        Err(e) => return (RC_ERROR, err_body(&format!("peer_ip parse: {e}"))),
    };

    let message = match base64::engine::general_purpose::STANDARD.decode(req.message_b64.as_bytes())
    {
        Ok(b) => b,
        Err(e) => return (RC_ERROR, err_body(&format!("message_b64 decode: {e}"))),
    };

    let account_id_str = req
        .account_id
        .as_ref()
        .map(stringify_account_id)
        .unwrap_or_default();
    let account = AccountId::new(account_id_str.clone());

    // C4 — substrate-resolved overrides. The wire `account_id` is a
    // stringified integer per the maild.accounts PK convention; parse
    // back to i32 to feed `db::account::get_by_id`. If account_id is
    // absent, unparseable, or the lookup misses, fall through to
    // defaults — explain is a diagnostic surface and refusing it on a
    // missing-account would prevent operators from running an explain
    // against an arbitrary recipient when the row was already deleted.
    // The metadata fields in the response (overrides_email,
    // overrides_resolved, overrides_version, overrides_nseq) tell the
    // operator which branch was taken.
    let resolved = resolve_overrides_for_explain(
        overrides_runtime,
        db,
        req.account_id.as_ref(),
        &account_id_str,
    )
    .await;

    // Synthesize an all-temperror VerifyResult — explain never does DNS.
    let verify = synthesize_temperror_verify(hostname, peer_ip, &req.envelope_from, "");

    let ctx = RuleContext {
        peer_ip,
        envelope_from: &req.envelope_from,
        envelope_to: &req.envelope_to,
        message: &message,
        mail_auth: &verify,
        sender_authenticated: req.sender_authenticated,
        account: &account,
        overrides: &resolved.overrides,
    };

    match engine.explain(&ctx).await {
        Ok(exp) => match serde_json::to_value(&exp) {
            Ok(mut body) => {
                if let serde_json::Value::Object(map) = &mut body {
                    map.insert(
                        "sender_authenticated".to_string(),
                        serde_json::Value::Bool(req.sender_authenticated),
                    );
                    map.insert(
                        "overrides_resolved".to_string(),
                        serde_json::Value::Bool(resolved.resolved),
                    );
                    map.insert(
                        "overrides_email".to_string(),
                        match resolved.email {
                            Some(e) => serde_json::Value::String(e),
                            None => serde_json::Value::Null,
                        },
                    );
                    map.insert(
                        "overrides_version".to_string(),
                        match resolved.version {
                            Some(v) => serde_json::Value::from(v),
                            None => serde_json::Value::Null,
                        },
                    );
                    map.insert(
                        "overrides_nseq".to_string(),
                        match resolved.nseq {
                            Some(v) => serde_json::Value::from(v),
                            None => serde_json::Value::Null,
                        },
                    );
                    if let Some(note) = resolved.note {
                        map.insert(
                            "overrides_note".to_string(),
                            serde_json::Value::String(note),
                        );
                    }
                }
                (0, body.to_string())
            }
            Err(e) => (RC_ERROR, err_body(&format!("serialize Explanation: {e}"))),
        },
        Err(e) => (RC_ERROR, err_body(&format!("explain failed: {e}"))),
    }
}

/// Internal explain-side resolver. Diagnostic surface — never errors,
/// always returns a `ResolvedExplainOverrides` whose `resolved` flag and
/// optional `note` describe which branch was taken so the operator can
/// distinguish "real override row at v=N" from "no row exists" from
/// "lookup failed; defaults used".
struct ResolvedExplainOverrides {
    overrides: AccountOverrides,
    resolved: bool,
    email: Option<String>,
    version: Option<u64>,
    nseq: Option<u64>,
    note: Option<String>,
}

async fn resolve_overrides_for_explain(
    overrides_runtime: &Arc<Runtime>,
    db: &db::Db,
    account_id_value: Option<&serde_json::Value>,
    account_id_str: &str,
) -> ResolvedExplainOverrides {
    let default = || ResolvedExplainOverrides {
        overrides: AccountOverrides::default(),
        resolved: false,
        email: None,
        version: None,
        nseq: None,
        note: None,
    };

    if account_id_value.is_none() || account_id_str.is_empty() {
        return ResolvedExplainOverrides {
            note: Some("no account_id supplied; using engine defaults".to_string()),
            ..default()
        };
    }

    let account_id: i32 = match account_id_str.parse() {
        Ok(n) => n,
        Err(_) => {
            return ResolvedExplainOverrides {
                note: Some(format!(
                    "account_id {account_id_str:?} not parseable as i32; using engine defaults"
                )),
                ..default()
            };
        }
    };

    let account = match db::account::get_by_id(&db.conn, account_id).await {
        Ok(Some(a)) => a,
        Ok(None) => {
            return ResolvedExplainOverrides {
                note: Some(format!(
                    "no maild.accounts row for id={account_id}; using engine defaults"
                )),
                ..default()
            };
        }
        Err(e) => {
            return ResolvedExplainOverrides {
                note: Some(format!(
                    "account lookup failed for id={account_id}: {e}; using engine defaults"
                )),
                ..default()
            };
        }
    };

    match resolve_account_overrides_by_email(overrides_runtime, &account.email).await {
        Ok(ResolvedOverrides {
            overrides,
            version,
            nseq,
        }) => {
            // `resolved` reflects "we found and used the account's
            // canonical overrides row state" — true for both a populated
            // record (version=Some) and a deliberate absent record
            // (version=None means schema defaults, which is the
            // post-Phase-2 baseline). It is only false on the early-
            // return defaulted branches above.
            //
            // Even on resolved=true, the absent-row sub-branch surfaces
            // a `note` so an operator reading the response sees the
            // branch explicitly rather than having to infer it from
            // `overrides_version: null`. That matches the diagnostic
            // requirement that every defaulting path be self-describing.
            let note = if version.is_none() {
                Some(format!(
                    "no maild.account_overrides row for {:?}; using schema defaults",
                    account.email
                ))
            } else {
                None
            };
            ResolvedExplainOverrides {
                overrides,
                resolved: true,
                email: Some(account.email),
                version,
                nseq,
                note,
            }
        }
        Err(e) => ResolvedExplainOverrides {
            note: Some(format!(
                "account_overrides read failed for {:?}: {e}; using engine defaults",
                account.email
            )),
            email: Some(account.email),
            ..default()
        },
    }
}

fn stringify_account_id(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

fn err_body(msg: &str) -> String {
    serde_json::json!({ "error": msg }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_maild_rules::EngineConfig;
    use cosmix_props::memory::MemoryStore;
    use cosmix_props::store::PropertyStore;
    use rusqlite::Connection;
    use std::sync::Mutex;
    use tempfile::tempdir;

    fn engine() -> Arc<DefaultRuleEngine> {
        let (e, _failed) = DefaultRuleEngine::with_pack_str(
            EngineConfig::default(),
            cosmix_maild_rules::default_pack_str(),
        )
        .expect("default pack must load");
        Arc::new(e)
    }

    /// C4 test fixture. Returns:
    /// - `overrides_runtime`: a real `Runtime` over a `MemoryStore`.
    ///   Tests that don't insert a row see the "absent → defaults"
    ///   branch in `resolve_account_overrides_by_email`.
    /// - `db`: a `db::Db` whose connection has the maild schema
    ///   applied but no accounts inserted. Tests that pass
    ///   `account_id: <n>` see the "no maild.accounts row" defaulting
    ///   branch in `resolve_overrides_for_explain`.
    ///
    /// The temporary directory backing the blob_dir is leaked into the
    /// test for the duration of the test run via tempdir().
    fn explain_fixtures() -> (Arc<Runtime>, db::Db, tempfile::TempDir) {
        use crate::props::account_overrides;
        let store: Arc<dyn PropertyStore> = Arc::new(MemoryStore::new("maild"));
        let spec = account_overrides::spec(cosmix_props::hooks::Hooks::noop());
        let overrides_runtime = Arc::new(Runtime::new("maild", spec, store));

        let conn = Connection::open_in_memory().expect("in-memory conn");
        conn.execute_batch(crate::db::SCHEMA)
            .expect("apply maild schema");
        let tmp = tempdir().expect("tempdir");
        let db_handle = db::Db {
            conn: Arc::new(Mutex::new(conn)),
            blob_dir: tmp.path().to_path_buf(),
        };
        (overrides_runtime, db_handle, tmp)
    }

    #[tokio::test]
    async fn stats_returns_zero_counters_after_construction() {
        let e = engine();
        let s = Arc::new(RuleStats::new());
        let (rc, body) = handle_stats(&e, &s, &serde_json::Value::Null).await;
        assert_eq!(rc, 0);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["verdicts_total"], 0);
        assert_eq!(v["shadow_mode"], false);
        assert_eq!(v["pack_version"].as_str().unwrap(), "v1.0");
        assert!(v["rules_loaded"].as_u64().unwrap() >= 1);
        assert_eq!(v["verdicts_by_kind"]["hard_accept"], 0);
        assert_eq!(v["verdicts_by_kind"]["hard_junk"], 0);
        assert_eq!(v["verdicts_by_kind"]["continue"], 0);
        // Commit 5: per-rule map present-but-empty on fresh state.
        assert!(v["per_rule"].is_object());
        assert_eq!(v["per_rule"].as_object().unwrap().len(), 0);
        assert_eq!(v["rules_with_hits"], 0);
        assert_eq!(v["per_rule_truncated"], false);
    }

    #[tokio::test]
    async fn stats_surfaces_per_rule_counters_after_hits() {
        let e = engine();
        let s = Arc::new(RuleStats::new());
        // Three hits on rule_a, one on rule_b — exercises both the
        // map shape and the count, without depending on which rules
        // the default pack actually exposes.
        let rule_a = "rule_a".to_string();
        let rule_b = "rule_b".to_string();
        s.record_rule_hit(&rule_a);
        s.record_rule_hit(&rule_a);
        s.record_rule_hit(&rule_a);
        s.record_rule_hit(&rule_b);

        let (rc, body) = handle_stats(&e, &s, &serde_json::Value::Null).await;
        assert_eq!(rc, 0);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let per_rule = v["per_rule"].as_object().expect("per_rule object");
        assert_eq!(per_rule.len(), 2);
        assert_eq!(per_rule["rule_a"], 3);
        assert_eq!(per_rule["rule_b"], 1);
        assert_eq!(v["rules_with_hits"], 2);
        assert_eq!(v["per_rule_truncated"], false);
    }

    #[tokio::test]
    async fn stats_top_n_caps_and_flags_truncation() {
        // Five rules with distinct counts; cap at 3 → the top-3 by
        // count are returned and `per_rule_truncated` is true.
        let e = engine();
        let s = Arc::new(RuleStats::new());
        for (id, n) in [
            ("r_a", 5u64),
            ("r_b", 4),
            ("r_c", 3),
            ("r_d", 2),
            ("r_e", 1),
        ] {
            let owned = id.to_string();
            for _ in 0..n {
                s.record_rule_hit(&owned);
            }
        }

        let args = serde_json::json!({ "top_n": 3 });
        let (rc, body) = handle_stats(&e, &s, &args).await;
        assert_eq!(rc, 0);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let per_rule = v["per_rule"].as_object().expect("per_rule object");
        assert_eq!(per_rule.len(), 3);
        // Top-3 by count must be present, the bottom-2 must not.
        assert_eq!(per_rule["r_a"], 5);
        assert_eq!(per_rule["r_b"], 4);
        assert_eq!(per_rule["r_c"], 3);
        assert!(!per_rule.contains_key("r_d"));
        assert!(!per_rule.contains_key("r_e"));
        // Total cardinality is still reported untruncated.
        assert_eq!(v["rules_with_hits"], 5);
        assert_eq!(v["per_rule_truncated"], true);
    }

    #[tokio::test]
    async fn stats_top_n_zero_returns_empty_map_with_truncation_flag() {
        // top_n=0 is a degenerate but well-defined request: return
        // no entries but still surface the untruncated cardinality
        // and flag the truncation. This lets callers cheaply ask
        // "how many distinct rules have ever matched?" without
        // paying for the map payload.
        let e = engine();
        let s = Arc::new(RuleStats::new());
        let r = "only_rule".to_string();
        s.record_rule_hit(&r);

        let args = serde_json::json!({ "top_n": 0 });
        let (rc, body) = handle_stats(&e, &s, &args).await;
        assert_eq!(rc, 0);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["per_rule"].as_object().unwrap().len(), 0);
        assert_eq!(v["rules_with_hits"], 1);
        assert_eq!(v["per_rule_truncated"], true);
    }

    #[tokio::test]
    async fn stats_top_n_above_max_clamps_silently() {
        // Asking for more than `MAX_PER_RULE_TOP_N` is clamped to
        // the ceiling — protects the wire payload regardless of
        // what the caller asks for.
        let e = engine();
        let s = Arc::new(RuleStats::new());
        let r = "only_rule".to_string();
        s.record_rule_hit(&r);

        let args = serde_json::json!({ "top_n": MAX_PER_RULE_TOP_N + 1 });
        let (rc, body) = handle_stats(&e, &s, &args).await;
        assert_eq!(rc, 0);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        // The single entry fits well under the clamped cap, so the
        // response is the full untruncated map.
        assert_eq!(v["per_rule"].as_object().unwrap().len(), 1);
        assert_eq!(v["per_rule_truncated"], false);
    }

    #[tokio::test]
    async fn stats_rejects_malformed_request_shapes() {
        // Covers both bad `top_n` values and bad request envelopes.
        // The single rejection path is `serde_json::from_value`, so
        // all of these surface as rc=10 with a "malformed stats
        // request" prefix.
        let e = engine();
        let s = Arc::new(RuleStats::new());
        let bad_inputs = [
            // Wrong top_n type.
            serde_json::json!({ "top_n": "ten" }),
            // Negative integer (top_n is u64).
            serde_json::json!({ "top_n": -1 }),
            // Fractional number.
            serde_json::json!({ "top_n": 1.5 }),
            // Non-object request shapes.
            serde_json::json!([]),
            serde_json::json!("bad"),
            serde_json::json!(42),
            // Unknown field — deny_unknown_fields keeps the contract
            // explicit and catches caller-side typos.
            serde_json::json!({ "topn": 10 }),
        ];
        for bad in bad_inputs {
            let (rc, body) = handle_stats(&e, &s, &bad).await;
            assert_eq!(
                rc, RC_ERROR,
                "expected RC_ERROR for {bad}; got body: {body}"
            );
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert!(
                v["error"]
                    .as_str()
                    .unwrap()
                    .starts_with("malformed stats request"),
                "error body should be prefixed by 'malformed stats request'; got: {body}"
            );
        }
    }

    #[tokio::test]
    async fn stats_accepts_empty_object_as_default_request() {
        // The doc shows `{}` as the canonical no-arg request shape.
        // Ensure it deserializes successfully (alongside Null, which
        // `resolve_args` yields when no header/body is provided).
        let e = engine();
        let s = Arc::new(RuleStats::new());
        let (rc, body) = handle_stats(&e, &s, &serde_json::json!({})).await;
        assert_eq!(rc, 0, "body was: {body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(v["per_rule"].is_object());
        assert_eq!(v["per_rule_truncated"], false);
    }

    #[tokio::test]
    async fn reload_with_no_pack_path_returns_current_state() {
        // The default-pack-str engine has no pack_path, so reload is
        // documented as a no-op that surfaces the current state.
        let e = engine();
        let (rc, body) = handle_reload(&e).await;
        assert_eq!(rc, 0);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["pack_version"].as_str().unwrap(), "v1.0");
        assert!(v["rules_loaded"].as_u64().unwrap() >= 1);
        assert!(v["rules_failed"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn explain_round_trips_a_minimal_message() {
        let e = engine();
        let (orx, db, _tmp) = explain_fixtures();
        let raw = b"From: sender@example.com\r\n\
                    To: dest@local\r\n\
                    Subject: hi\r\n\
                    \r\n\
                    body\r\n";
        let b64 = base64::engine::general_purpose::STANDARD.encode(raw);
        let args = serde_json::json!({
            "account_id": 7,
            "envelope_from": "sender@example.com",
            "envelope_to": ["dest@local"],
            "peer_ip": "192.0.2.1",
            "message_b64": b64,
            "mail_auth": null,
        });
        let (rc, body) = handle_explain(&e, &args, "test.example", &orx, &db).await;
        assert_eq!(rc, 0, "body was: {body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(v["rules"].is_array());
        assert!(v["pack_version"].as_str().is_some());
        assert!(v["threshold"].as_f64().is_some());
        // Default config + temperror auth + minimal message → Continue.
        assert_eq!(v["verdict"].as_str().unwrap(), "Continue");
        // C4: no maild.accounts row for id=7 in the fixture, so the
        // resolver took the defaulting branch with a note. Metadata
        // fields are surfaced even on the defaulting branch.
        assert_eq!(v["overrides_resolved"], serde_json::Value::Bool(false));
        assert!(v["overrides_note"].as_str().unwrap().contains("id=7"));
        assert!(v["overrides_version"].is_null());
        assert!(v["overrides_nseq"].is_null());
    }

    #[tokio::test]
    async fn explain_without_account_id_uses_engine_defaults_without_db_lookup() {
        // C4: omitting `account_id` skips the substrate lookup entirely
        // — the note reflects "no account_id supplied", not a DB miss.
        let e = engine();
        let (orx, db, _tmp) = explain_fixtures();
        let raw = b"From: a@b\r\n\r\nx\r\n";
        let b64 = base64::engine::general_purpose::STANDARD.encode(raw);
        let args = serde_json::json!({
            "envelope_from": "a@b",
            "envelope_to": ["dest@local"],
            "peer_ip": "192.0.2.1",
            "message_b64": b64,
            "mail_auth": null,
        });
        let (rc, body) = handle_explain(&e, &args, "test.example", &orx, &db).await;
        assert_eq!(rc, 0, "body was: {body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["overrides_resolved"], serde_json::Value::Bool(false));
        assert!(
            v["overrides_note"]
                .as_str()
                .unwrap()
                .contains("no account_id supplied")
        );
        assert!(v["overrides_email"].is_null());
    }

    #[tokio::test]
    async fn explain_surfaces_substrate_metadata_for_real_account() {
        // C4: when both the account row AND a substrate override row
        // exist, the response carries the row's version + observed
        // nseq so an operator can correlate the verdict to an audit
        // entry. Uses a real account inserted into the fixture DB and
        // a real `runtime.set` to populate the overrides namespace.
        use cosmix_props::record::{Actor, RecordKey};
        use cosmix_props::runtime::SetOpts;
        use cosmix_props::store::MergeMode;
        use cosmix_props::value::PropValue;

        let e = engine();
        let (orx, db, _tmp) = explain_fixtures();

        // Seed an accounts row.
        {
            let conn = db.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO accounts (id, email, password) VALUES (?1, ?2, ?3)",
                rusqlite::params![42, "u@example.com", "x"],
            )
            .unwrap();
        }

        // Seed the override row through the substrate so version/nseq
        // are real, not synthesized.
        let key = RecordKey::collection(
            crate::props::account_overrides::namespace_name(),
            "u@example.com",
        );
        let mut m = std::collections::BTreeMap::new();
        m.insert("email".into(), PropValue::String("u@example.com".into()));
        m.insert("threshold_override".into(), PropValue::Float(7.5));
        let opts = SetOpts {
            expected_version: None,
            merge: MergeMode::Replace,
            actor: Actor::service("maild-tests").expect("valid actor"),
            cause: None,
            ts_ms: 0,
        };
        let _out = orx.set(key, PropValue::Object(m), opts).await.unwrap();

        let b64 = base64::engine::general_purpose::STANDARD.encode(b"From: a@b\r\n\r\nx\r\n");
        let args = serde_json::json!({
            "account_id": 42,
            "envelope_from": "a@b",
            "envelope_to": ["dest@local"],
            "peer_ip": "192.0.2.1",
            "message_b64": b64,
            "mail_auth": null,
        });
        let (rc, body) = handle_explain(&e, &args, "test.example", &orx, &db).await;
        assert_eq!(rc, 0, "body was: {body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["overrides_resolved"], serde_json::Value::Bool(true));
        assert_eq!(
            v["overrides_email"],
            serde_json::Value::String("u@example.com".into())
        );
        assert!(v["overrides_version"].as_u64().is_some());
        assert!(v["overrides_nseq"].as_u64().is_some());
        // C4 also surfaces the threshold from the override (not the
        // engine default) into the explain's `threshold` field.
        assert_eq!(v["threshold"].as_f64().unwrap(), 7.5);
    }

    #[tokio::test]
    async fn explain_account_present_without_override_row_notes_the_branch() {
        // C4 diagnostic-surface contract: when the account row exists
        // but no maild.account_overrides row has been written, the
        // resolver returns schema defaults and `version=None`. The
        // response must still mark `overrides_resolved=true` (the
        // canonical state was successfully resolved) AND surface a
        // descriptive note so an operator does not have to infer the
        // branch from `overrides_version: null`.
        let e = engine();
        let (orx, db, _tmp) = explain_fixtures();

        // Seed an accounts row, but DO NOT write an overrides row.
        {
            let conn = db.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO accounts (id, email, password) VALUES (?1, ?2, ?3)",
                rusqlite::params![99, "noov@example.com", "x"],
            )
            .unwrap();
        }

        let b64 = base64::engine::general_purpose::STANDARD.encode(b"From: a@b\r\n\r\nx\r\n");
        let args = serde_json::json!({
            "account_id": 99,
            "envelope_from": "a@b",
            "envelope_to": ["dest@local"],
            "peer_ip": "192.0.2.1",
            "message_b64": b64,
            "mail_auth": null,
        });
        let (rc, body) = handle_explain(&e, &args, "test.example", &orx, &db).await;
        assert_eq!(rc, 0, "body was: {body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["overrides_resolved"], serde_json::Value::Bool(true));
        assert_eq!(
            v["overrides_email"],
            serde_json::Value::String("noov@example.com".into())
        );
        assert!(v["overrides_version"].is_null());
        assert!(v["overrides_nseq"].is_null());
        let note = v["overrides_note"].as_str().expect("note must be present");
        assert!(
            note.contains("no maild.account_overrides row"),
            "note was: {note}"
        );
    }

    #[tokio::test]
    async fn explain_rejects_bad_base64() {
        let e = engine();
        let (orx, db, _tmp) = explain_fixtures();
        let args = serde_json::json!({
            "envelope_from": "x@y",
            "envelope_to": [],
            "peer_ip": "192.0.2.1",
            "message_b64": "***not base64***",
            "mail_auth": null,
        });
        let (rc, body) = handle_explain(&e, &args, "test.example", &orx, &db).await;
        assert_eq!(rc, RC_ERROR);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(v["error"].as_str().unwrap().contains("decode"));
    }

    #[tokio::test]
    async fn explain_rejects_bad_peer_ip() {
        let e = engine();
        let (orx, db, _tmp) = explain_fixtures();
        let args = serde_json::json!({
            "envelope_from": "x@y",
            "envelope_to": [],
            "peer_ip": "not-an-ip",
            "message_b64": base64::engine::general_purpose::STANDARD.encode(b""),
            "mail_auth": null,
        });
        let (rc, body) = handle_explain(&e, &args, "test.example", &orx, &db).await;
        assert_eq!(rc, RC_ERROR);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(v["error"].as_str().unwrap().contains("peer_ip"));
    }

    #[tokio::test]
    async fn explain_rejects_pre_verified_mail_auth() {
        let e = engine();
        let (orx, db, _tmp) = explain_fixtures();
        let args = serde_json::json!({
            "envelope_from": "x@y",
            "envelope_to": [],
            "peer_ip": "192.0.2.1",
            "message_b64": base64::engine::general_purpose::STANDARD.encode(b""),
            "mail_auth": {"some": "object"},
        });
        let (rc, body) = handle_explain(&e, &args, "test.example", &orx, &db).await;
        assert_eq!(rc, RC_ERROR);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(v["error"].as_str().unwrap().contains("Phase 2"));
    }

    fn cmd_with(
        body: &str,
        args_header: Option<&str>,
        parsed_args: serde_json::Value,
    ) -> IncomingCommand {
        let mut headers = std::collections::BTreeMap::new();
        if let Some(h) = args_header {
            headers.insert("args".to_string(), h.to_string());
        }
        IncomingCommand {
            from: "test".into(),
            command: "maild.rules.bogus".into(),
            id: None,
            args: parsed_args,
            body: body.into(),
            headers,
        }
    }

    #[tokio::test]
    async fn dispatch_unknown_action_returns_rc_error() {
        let e = engine();
        let s = Arc::new(RuleStats::new());
        let (orx, db, _tmp) = explain_fixtures();
        let cmd = cmd_with("", None, serde_json::Value::Null);
        let (rc, body) = dispatch("bogus", &cmd, &e, &s, "h", &orx, &db).await;
        assert_eq!(rc, RC_ERROR);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(
            v["error"]
                .as_str()
                .unwrap()
                .contains("unknown rules action")
        );
    }

    #[tokio::test]
    async fn dispatch_explain_resolves_args_from_args_header() {
        // Spec-shaped caller: parameters arrive in the `args:` header
        // with body and `cmd.args` empty. Without header parsing, the
        // explain request would be malformed and the call would fail.
        let e = engine();
        let s = Arc::new(RuleStats::new());
        let raw = b"From: a@b\r\n\r\nhi\r\n";
        let b64 = base64::engine::general_purpose::STANDARD.encode(raw);
        let args_json = serde_json::json!({
            "envelope_from": "a@b",
            "envelope_to": ["dest@local"],
            "peer_ip": "192.0.2.1",
            "message_b64": b64,
            "mail_auth": null,
        })
        .to_string();
        let (orx, db, _tmp) = explain_fixtures();
        let cmd = cmd_with("", Some(&args_json), serde_json::Value::Null);
        let (rc, body) = dispatch("explain", &cmd, &e, &s, "test.example", &orx, &db).await;
        assert_eq!(rc, 0, "body was: {body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(v["rules"].is_array());
        assert_eq!(v["verdict"].as_str().unwrap(), "Continue");
    }

    #[tokio::test]
    async fn dispatch_explain_falls_back_to_body_then_cmd_args() {
        // Body-carried JSON should win when no `args:` header is present.
        let e = engine();
        let s = Arc::new(RuleStats::new());
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"From: a@b\r\n\r\nx\r\n");
        let body_json = serde_json::json!({
            "envelope_from": "a@b",
            "envelope_to": [],
            "peer_ip": "192.0.2.1",
            "message_b64": b64,
            "mail_auth": null,
        })
        .to_string();
        // cmd.args carries the body parsed as JSON, matching how
        // NodedClient populates IncomingCommand from a body.
        let parsed: serde_json::Value = serde_json::from_str(&body_json).unwrap();
        let (orx, db, _tmp) = explain_fixtures();
        let cmd = cmd_with(&body_json, None, parsed);
        let (rc, _) = dispatch("explain", &cmd, &e, &s, "test.example", &orx, &db).await;
        assert_eq!(rc, 0);
    }

    #[tokio::test]
    async fn dispatch_explain_args_header_takes_precedence_over_body() {
        // If both are populated and they disagree, the header wins —
        // it is the documented carrier for command parameters.
        let e = engine();
        let s = Arc::new(RuleStats::new());
        let good_b64 = base64::engine::general_purpose::STANDARD.encode(b"From: a@b\r\n\r\nx\r\n");
        let header_args = serde_json::json!({
            "envelope_from": "header@example",
            "envelope_to": [],
            "peer_ip": "192.0.2.1",
            "message_b64": good_b64,
            "mail_auth": null,
        })
        .to_string();
        // Body carries a request with bad peer_ip — if it were used,
        // the call would fail with rc=10. Successful rc=0 proves the
        // header took precedence.
        let bad_body = serde_json::json!({
            "envelope_from": "body@example",
            "envelope_to": [],
            "peer_ip": "not-an-ip",
            "message_b64": "",
            "mail_auth": null,
        })
        .to_string();
        let (orx, db, _tmp) = explain_fixtures();
        let cmd = cmd_with(&bad_body, Some(&header_args), serde_json::Value::Null);
        let (rc, body) = dispatch("explain", &cmd, &e, &s, "test.example", &orx, &db).await;
        assert_eq!(rc, 0, "body was: {body}");
    }

    #[tokio::test]
    async fn dispatch_stats_rejects_malformed_json_in_args_header() {
        // The loose `resolve_args` helper silently drops an
        // unparsable `args:` header to `Value::Null` and lets the
        // verb succeed with defaults. For `stats` we want a hard
        // rc=10 instead, so dispatch routes through
        // `try_resolve_args`. Verify that path here.
        let e = engine();
        let s = Arc::new(RuleStats::new());
        let (orx, db, _tmp) = explain_fixtures();
        let cmd = cmd_with("", Some("{not json"), serde_json::Value::Null);
        let (rc, body) = dispatch("stats", &cmd, &e, &s, "h", &orx, &db).await;
        assert_eq!(rc, RC_ERROR, "body was: {body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let err = v["error"].as_str().unwrap();
        assert!(
            err.starts_with("malformed stats request"),
            "error body should be prefixed by 'malformed stats request'; got: {body}"
        );
        assert!(
            err.contains("args header"),
            "error body should identify the failing source; got: {body}"
        );
    }

    #[tokio::test]
    async fn dispatch_stats_rejects_malformed_json_in_body() {
        let e = engine();
        let s = Arc::new(RuleStats::new());
        let (orx, db, _tmp) = explain_fixtures();
        let cmd = cmd_with("{not json", None, serde_json::Value::Null);
        let (rc, body) = dispatch("stats", &cmd, &e, &s, "h", &orx, &db).await;
        assert_eq!(rc, RC_ERROR, "body was: {body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let err = v["error"].as_str().unwrap();
        assert!(err.starts_with("malformed stats request"));
        assert!(err.contains("body"));
    }

    #[tokio::test]
    async fn dispatch_stats_succeeds_with_no_args() {
        // No header, no body, cmd.args = Null → falls through to
        // the default request shape. This is the canonical zero-arg
        // call the doc shows as `{}`.
        let e = engine();
        let s = Arc::new(RuleStats::new());
        let (orx, db, _tmp) = explain_fixtures();
        let cmd = cmd_with("", None, serde_json::Value::Null);
        let (rc, body) = dispatch("stats", &cmd, &e, &s, "h", &orx, &db).await;
        assert_eq!(rc, 0, "body was: {body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(v["per_rule"].is_object());
        assert_eq!(v["per_rule_truncated"], false);
    }
}
