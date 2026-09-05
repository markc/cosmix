//! SPEC 12 property-substrate namespace `maild.aliases` — Phase 1
//! local-target single-hop email aliases.
//!
//! A row maps an **alias address** (the key) to a **target** local
//! account email, so mail addressed to the alias is delivered to the
//! target's mailbox AND an authenticated account may submit `From:` the
//! alias (the alias is an identity *of* the target). Both directions go
//! through the single resolver [`resolve_alias`]; see the plan
//! `~/.cosmix/_plan/2026-06-09-maild-aliases.md`.
//!
//! ## Lifecycle
//!
//! `Simple` — no provisioning side-effects; the substrate commits the
//! validated body into `__props_values` via [`JsonValuesMapping`].
//!
//! ## Canonical form
//!
//! [`canon`] (`trim` + ASCII-lowercase) is applied to the key, the body
//! `target`, and **every** lookup, so the alias key and the recipient/
//! sender comparison never disagree (SMTP lowercases addresses but
//! `db::account::get_by_email` is an exact SQL match — this is the one
//! contract that closes that gap).
//!
//! ## No-chaining invariant (Phase 1 cycle/chain foreclosure)
//!
//! `before_set` enforces that the set of alias **keys** and the set of
//! alias **targets** stay **disjoint**: an address may be an alias key
//! OR a target, never both. With that invariant no alias's target is
//! itself an alias, so resolution is always single-hop and acyclic — no
//! resolve-time hop cap is needed. Multi-hop (with a bounded cap) is a
//! deferred Phase 2 item.

use std::sync::Arc;

use anyhow::Result;
use cosmix_props::bus::mutation::PropsRouter;
use cosmix_props::capability::{Capability, CapabilitySet};
use cosmix_props::hooks::{HookCtx, HookError, HookFuture, HookHandler, Hooks};
use cosmix_props::namespace::{
    AuthPolicy, Cardinality, FieldSchema, FieldType, NamespaceLifecycle, NamespaceName,
    NamespaceSpec, PeerIdentity, PropertySchema, StorageBackendKind,
};
use cosmix_props::record::RecordKey;
use cosmix_props::runtime::Runtime;
use cosmix_props::sqlite::{JsonValuesMapping, SqliteStore};
use cosmix_props::value::PropValue;

/// Unqualified namespace name; fully qualified is `maild.aliases`.
pub const NAMESPACE: &str = "aliases";

pub fn namespace_name() -> NamespaceName {
    NamespaceName::new(NAMESPACE).expect("constant namespace name is valid")
}

/// Canonical address form — the single rule applied to the key, the
/// stored `target`, and every resolve/compare. ASCII-lowercase + trim;
/// byte-exact (no Unicode normalization, consistent with the rest of
/// maild's address handling).
pub fn canon(addr: &str) -> String {
    addr.trim().to_ascii_lowercase()
}

/// SPEC 12 schema. `address` is the primary-key body field (the runtime
/// enforces `value.address == wire-key`).
pub fn schema() -> PropertySchema {
    PropertySchema::new(vec![
        FieldSchema {
            name: "address".into(),
            ty: FieldType::Email,
            default: None,
            secret: false,
            help: "The alias address (primary key, canonical lowercase)".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "target".into(),
            ty: FieldType::Email,
            default: None,
            secret: false,
            help: "Local account email the alias delivers to / submits as".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "enabled".into(),
            ty: FieldType::Bool,
            default: Some(PropValue::Bool(true)),
            secret: false,
            help: "Whether the alias is active (disabled rows resolve to nothing)".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
    ])
}

/// SPEC 12 [`AuthPolicy`] — grants every property capability to every
/// reachable peer, the same WireGuard-`/24`-is-the-trust-domain posture
/// as `maild.accounts` / `maild.account_overrides`. (A narrower
/// operator tier is a future hardening, tracked with the sibling
/// namespaces, not introduced piecemeal here.)
pub fn auth_policy() -> AuthPolicy {
    fn resolve(_peer: &PeerIdentity) -> CapabilitySet {
        [
            "props.read:maild.aliases",
            "props.write:maild.aliases",
            "props.describe:maild.aliases:public",
            "props.describe:maild.aliases:full",
            "props.audit:maild.aliases",
        ]
        .into_iter()
        .map(|s| Capability::new(s).expect("non-empty capability"))
        .collect()
    }
    AuthPolicy::new(resolve)
}

pub fn spec(hooks: Hooks) -> NamespaceSpec {
    let mut s = NamespaceSpec::new(
        namespace_name(),
        schema(),
        Cardinality::Collection {
            primary_key_field: "address".into(),
        },
        StorageBackendKind::SqliteTable {
            table: "__props_values".into(),
        },
    );
    s.lifecycle = NamespaceLifecycle::Simple;
    s.auth = auth_policy();
    s.hooks = hooks;
    s
}

/// Hook handler. `before_set` validates address/target shape, the
/// canonical-form contract, self-alias, and the **account/alias
/// disjointness**: an alias KEY must not be a real account (no shadowing)
/// and the TARGET must BE a real account.
///
/// That disjointness also **subsumes any no-chaining check**: every alias
/// key is a non-account and every target is an account, and the
/// reciprocal guard in `props::accounts` keeps accounts out of the
/// alias-key set — so a target (account) is never an alias key, hence no
/// alias can point at another alias. Resolution is single-hop regardless.
///
/// `accounts_runtime` is a direct `Arc` (accounts registers BEFORE
/// aliases, so it exists at construction) — the same shape
/// `account_overrides` uses to read `accounts`.
pub struct AliasesHooks {
    accounts_runtime: Arc<Runtime>,
}

impl AliasesHooks {
    pub fn new(accounts_runtime: Arc<Runtime>) -> Self {
        Self { accounts_runtime }
    }

    /// True if `email` (canonical) has a live `maild.accounts` row.
    async fn account_exists(&self, email: &str) -> Result<bool, HookError> {
        let key =
            RecordKey::collection(crate::props::accounts::namespace_name(), email.to_string());
        match self.accounts_runtime.store().get(&key).await {
            Ok(_) => Ok(true),
            Err(cosmix_props::store::StoreError::NotFound) => Ok(false),
            Err(e) => Err(HookError::hook(format!(
                "aliases->accounts cross-namespace read failed: {e}"
            ))),
        }
    }

    fn validate_email_shape(field: &str, s: &str) -> Result<(), HookError> {
        if s.is_empty() {
            return Err(HookError::validation(format!("{field} is empty")));
        }
        if s.chars().any(char::is_whitespace) {
            return Err(HookError::validation(format!(
                "{field} contains whitespace"
            )));
        }
        let mut parts = s.split('@');
        let local = parts.next().unwrap_or("");
        let domain = parts.next().unwrap_or("");
        if parts.next().is_some() {
            return Err(HookError::validation(format!(
                "{field} has multiple @ separators"
            )));
        }
        if local.is_empty() || domain.is_empty() {
            return Err(HookError::validation(format!(
                "{field} must have non-empty local and domain parts"
            )));
        }
        if !domain.contains('.') {
            return Err(HookError::validation(format!(
                "{field} domain must contain a dot"
            )));
        }
        // Reserve the opaque vtoken namespace (C9): a 6/10-char Crockford-base32
        // local-part is a capability-token shape, NOT an alias name.
        if crate::vtoken::is_opaque_shaped(local) {
            return Err(HookError::validation(format!(
                "{field} local-part matches the reserved opaque vtoken shape (6/10 Crockford-base32)"
            )));
        }
        Ok(())
    }
}

impl HookHandler for AliasesHooks {
    fn before_set<'a>(&'a self, ctx: &'a HookCtx) -> HookFuture<'a, ()> {
        Box::pin(async move {
            // Wire key shape + canonical-form contract: the key IS the
            // alias address and must already be canonical so the stored
            // form matches every `resolve_alias(canon(addr))` lookup.
            Self::validate_email_shape("alias address", &ctx.key.key)?;
            if ctx.key.key != canon(&ctx.key.key) {
                return Err(HookError::validation(format!(
                    "alias address {:?} is not canonical (expected lowercase/trimmed {:?})",
                    ctx.key.key,
                    canon(&ctx.key.key)
                )));
            }

            // Target shape + canonical form. Read from `new` (Replace) or
            // the merged-with-old view is unnecessary here because both
            // address and target are required fields with no schema
            // default, so a valid create/replace always carries `target`.
            let Some(new) = &ctx.new else {
                // A delete (new = None) needs no body validation.
                return Ok(());
            };
            let obj = new
                .as_object()
                .ok_or_else(|| HookError::validation("aliases record must be an object"))?;
            let target = match obj.get("target") {
                Some(PropValue::String(s)) => s.clone(),
                Some(other) => {
                    return Err(HookError::validation(format!(
                        "target must be a string email (got {})",
                        other.type_name()
                    )));
                }
                None => return Err(HookError::validation("target is required")),
            };
            Self::validate_email_shape("target", &target)?;
            if target != canon(&target) {
                return Err(HookError::validation(format!(
                    "target {target:?} is not canonical (expected {:?})",
                    canon(&target)
                )));
            }

            // Self-alias: a row that points to itself is a no-op loop.
            if ctx.key.key == target {
                return Err(HookError::validation(
                    "alias address and target are identical (self-alias)",
                ));
            }

            // Account/alias disjointness. Without these an alias could
            // SHADOW a real account — `mc@` could be both a live account
            // and an alias `mc@ → eve@`, so the resolve-before-lookup
            // paths would deliver mc@'s mail to eve@ AND let eve@ submit
            // `From: mc@`. So:
            //   * the alias KEY must NOT be a real account (no shadowing);
            //   * the TARGET MUST be a real account (the contract — mail
            //     to the alias has somewhere live to land).
            // (The reciprocal "an account email must not be an alias key"
            // is enforced on the accounts side in `props::accounts`.)
            if self.account_exists(&ctx.key.key).await? {
                return Err(HookError::validation(format!(
                    "alias address {:?} is already a real account — an address cannot be \
                     both an account and an alias (it would shadow the account's mail)",
                    ctx.key.key
                )));
            }
            if !self.account_exists(&target).await? {
                return Err(HookError::validation(format!(
                    "alias target {target:?} is not a maild.accounts row — an alias must \
                     point to a real local account"
                )));
            }

            // No explicit no-chaining check is needed: the two account
            // guards above (key ∉ accounts, target ∈ accounts) plus the
            // reciprocal account-side guard (account ∉ alias keys) keep
            // the alias-key and account sets disjoint, so a target (an
            // account) can never be an alias key — no alias can point at
            // another alias.
            //
            // KNOWN LIMITATION (accepted, Phase 1): these are
            // read-then-commit cross-namespace checks, so they carry the
            // substrate's project-wide hook-model TOCTOU — the SAME window
            // `account_overrides` lives with against `accounts`. A
            // *concurrent* `alias add a@` and `account add a@` could both
            // pass before either commits, leaving `a@` as both an account
            // AND an alias key. The blast radius in that state is a REAL
            // shadow, not just a dangling alias: mail to `a@` resolves to
            // the alias target, and that target can submit `From: a@`,
            // while `a@` is also a live account. We accept it for Phase 1
            // ONLY because reaching it needs two privileged mutations
            // racing on the SAME address (a single operator running CLI
            // commands sequentially never hits it; there is no external
            // caller path), and resolution is single-hop so it can't loop
            // or multi-hop. It is operator-CORRECTABLE but NOT
            // self-healing: an account *update* skips this check
            // (`ctx.old.is_some()`), so a dual-state persists until an
            // operator removes/repoints the alias or deletes one side —
            // which `reconcile_diagnostic` (run at startup) surfaces
            // loudly so it can't sit silently across a restart. Atomic
            // enforcement is a substrate-level concern (cross-namespace
            // mutation serialization / DB-backed constraint, benefiting
            // every namespace) and is deferred to that hardening; see the
            // follow-up note in the plan. Cascade-on-account-delete is
            // likewise deferred.
            Ok(())
        })
    }
}

/// Wire the `maild.aliases` namespace into the substrate. Returns the
/// runtime; the caller populates the passed `OnceLock` with it after
/// registration so `before_set`'s self-list can resolve.
pub fn register(
    router: &mut PropsRouter,
    store: &Arc<SqliteStore>,
    accounts_runtime: Arc<Runtime>,
) -> Result<Arc<Runtime>> {
    let hooks = Hooks::new(AliasesHooks::new(accounts_runtime));
    let spec = spec(hooks);
    store
        .register_namespace(&spec, Arc::new(JsonValuesMapping::new(namespace_name())))
        .map_err(|e| anyhow::anyhow!("register aliases namespace in store: {e}"))?;
    let runtime = Arc::new(Runtime::new(router.service(), spec, store.clone()));
    router
        .register(runtime.clone())
        .map_err(|e| anyhow::anyhow!("register aliases runtime on router: {e}"))?;
    Ok(runtime)
}

/// Resolve `addr` through the alias table. `Ok(Some(target))` if `addr`
/// is an enabled alias, `Ok(None)` if it is genuinely not an alias
/// (missing or disabled row), and `Err` on a **transient store read
/// failure** — callers MUST distinguish the two so a transient fault
/// doesn't masquerade as "not an alias" (an inbound recipient would then
/// hard-`550` instead of a retriable `451`; an outbound sender must
/// fail closed). Single-hop by construction: exactly one lookup, never
/// follows the target onward, so a (racily-committed) cyclic/chained row
/// can never loop or fan out here.
pub async fn resolve_alias(runtime: &Arc<Runtime>, addr: &str) -> Result<Option<String>> {
    let key = RecordKey::collection(namespace_name(), canon(addr));
    // `store().get()` yields a snapshot whose `.value` is the `Record`;
    // the record's own `.value` is the `PropValue` body. NotFound is a
    // clean "not an alias"; any other error is transient.
    let snap = match runtime.store().get(&key).await {
        Ok(s) => s,
        Err(cosmix_props::store::StoreError::NotFound) => return Ok(None),
        Err(e) => return Err(anyhow::anyhow!("aliases store read failed: {e}")),
    };
    let Some(obj) = snap.value.value.as_object() else {
        return Ok(None);
    };
    let enabled = match obj.get("enabled") {
        Some(PropValue::Bool(b)) => *b,
        // Field absent ⇒ schema default (true).
        None => true,
        _ => return Ok(None),
    };
    if !enabled {
        return Ok(None);
    }
    Ok(match obj.get("target") {
        Some(PropValue::String(t)) => Some(canon(t)),
        _ => None,
    })
}

/// Rewrite-before-lookup helper for the inbound recipient sites: the
/// canonical alias target if `addr` is an alias, else the **original**
/// `addr` unchanged. Falling back to the original (not `canon(addr)`)
/// keeps the non-alias path byte-identical to the pre-alias code — only
/// an actual alias rewrites the recipient. Propagates a transient
/// store error (`Err`) so inbound callers can return a retriable reject
/// rather than silently treating the recipient as a non-alias.
pub async fn resolve_recipient(runtime: &Arc<Runtime>, addr: &str) -> Result<String> {
    Ok(resolve_alias(runtime, addr)
        .await?
        .unwrap_or_else(|| addr.to_string()))
}

/// True if `from` is an authorized sender for the authenticated account
/// `authed` — its own address, or an enabled alias whose target is that
/// account. Both compared in canonical form. **Fails closed**: a
/// transient alias-store error returns `false` (an unverifiable sender
/// is not authorized — never lets a `From` through on a read fault).
pub async fn sender_authorized(runtime: &Arc<Runtime>, from: &str, authed: &str) -> bool {
    let from_c = canon(from);
    let authed_c = canon(authed);
    if from_c == authed_c {
        return true;
    }
    matches!(resolve_alias(runtime, &from_c).await, Ok(Some(t)) if t == authed_c)
}

/// True if `email` (canonical) has a `maild.accounts` row. Best-effort:
/// a transient read error returns `false` (used only by the diagnostic).
async fn account_row_exists(accounts_runtime: &Arc<Runtime>, email: &str) -> bool {
    let key = RecordKey::collection(crate::props::accounts::namespace_name(), email.to_string());
    accounts_runtime.store().get(&key).await.is_ok()
}

/// Startup reconciliation diagnostic for the account/alias disjointness
/// invariant (see the TOCTOU note in `before_set`). Walks every alias row
/// and logs LOUDLY when it finds a violation that a concurrent-write race
/// could have left behind:
///
/// * **shadow** — the alias KEY is also a live account, so mail to that
///   address resolves to the alias target and the target can submit
///   `From:` it, hijacking the real account;
/// * **dangling** — the alias TARGET is not a live account, so mail to
///   the alias bounces.
///
/// Informational only: the daemon does NOT auto-repair (no silent
/// mutation of operator intent). The operator fixes it with
/// `cosmix-maild alias remove` / `add`. Running this at startup stops a
/// raced dual-state from sitting silently across a restart.
/// Detection half of [`reconcile_diagnostic`], split out so it is unit-
/// testable without capturing logs. Returns `(shadows, dangling)` where
/// `shadows` are alias keys that are ALSO accounts and `dangling` are
/// `(alias_key, target)` pairs whose target is not an account.
async fn compute_alias_conflicts(
    aliases_runtime: &Arc<Runtime>,
    accounts_runtime: &Arc<Runtime>,
) -> Result<(Vec<String>, Vec<(String, String)>)> {
    let snap = aliases_runtime
        .store()
        .list(&namespace_name())
        .await
        .map_err(|e| anyhow::anyhow!("list maild.aliases for reconciliation: {e}"))?;
    let mut shadows: Vec<String> = Vec::new();
    let mut dangling: Vec<(String, String)> = Vec::new();
    for record in &snap.value {
        let key = record.key.key.clone();
        if account_row_exists(accounts_runtime, &key).await {
            shadows.push(key.clone());
        }
        if let PropValue::Object(m) = &record.value
            && let Some(PropValue::String(t)) = m.get("target")
        {
            let target = canon(t);
            if !account_row_exists(accounts_runtime, &target).await {
                dangling.push((key, target));
            }
        }
    }
    Ok((shadows, dangling))
}

pub async fn reconcile_diagnostic(
    aliases_runtime: &Arc<Runtime>,
    accounts_runtime: &Arc<Runtime>,
) -> Result<()> {
    let (shadows, dangling) = compute_alias_conflicts(aliases_runtime, accounts_runtime).await?;
    if shadows.is_empty() && dangling.is_empty() {
        tracing::info!("maild.aliases reconciliation: clean (no account/alias conflicts)");
    } else {
        tracing::warn!(
            ?shadows,
            ?dangling,
            "maild.aliases reconciliation found account/alias conflicts — repair via \
             `cosmix-maild alias remove`/`add`. SHADOWS (alias key is also a real account) \
             hijack that account's mail + From-identity; DANGLING (target is not an account) \
             bounce. These can only arise from a privileged concurrent-write race on one \
             address (see the before_set TOCTOU note)."
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_props::record::{Actor, Version};
    use cosmix_props::runtime::SetOpts;
    use cosmix_props::sqlite::SqliteStore;
    use cosmix_props::store::MergeMode;
    use rusqlite::Connection;
    use std::collections::BTreeMap;

    #[test]
    fn canon_lowercases_and_trims() {
        assert_eq!(canon("  MC@Example.COM "), "mc@example.com");
        assert_eq!(canon("admin@example.com"), "admin@example.com");
    }

    fn ctx(key: &str, body: Option<&[(&str, PropValue)]>) -> HookCtx {
        let new = body.map(|pairs| {
            let mut m = BTreeMap::new();
            for (k, v) in pairs {
                m.insert((*k).to_string(), v.clone());
            }
            PropValue::Object(m)
        });
        HookCtx {
            key: RecordKey::collection(namespace_name(), key),
            old: None,
            new,
            version: Version::zero(),
            actor: Actor::service("test").expect("valid actor"),
            merge: Some(MergeMode::Replace),
            origin: cosmix_props::WriteOrigin::caller(),
        }
    }

    fn body(target: &str) -> Vec<(&'static str, PropValue)> {
        // address is filled by the caller's key; leak a 'static-ish set.
        vec![
            ("target", PropValue::String(target.to_string())),
            ("enabled", PropValue::Bool(true)),
        ]
    }

    /// Stand-in `maild.accounts` namespace over `store`, seeded with
    /// `emails` as existing accounts. `account_exists` only does a
    /// `store().get()` / NotFound on the accounts namespace key, so a
    /// trivial namespace registered under `accounts::namespace_name()` is
    /// a faithful substitute for the real (Saga + mailstore) accounts
    /// namespace in these unit tests.
    async fn standin_accounts(
        store: &Arc<SqliteStore>,
        router: &mut PropsRouter,
        emails: &[&str],
    ) -> Arc<Runtime> {
        let schema = PropertySchema::new(vec![FieldSchema {
            name: "email".into(),
            ty: FieldType::Email,
            default: None,
            secret: false,
            help: String::new(),
            since: None,
            until: None,
            validators: Vec::new(),
        }]);
        let mut spec = NamespaceSpec::new(
            crate::props::accounts::namespace_name(),
            schema,
            Cardinality::Collection {
                primary_key_field: "email".into(),
            },
            StorageBackendKind::SqliteTable {
                table: "__props_values".into(),
            },
        );
        spec.lifecycle = NamespaceLifecycle::Simple;
        spec.hooks = Hooks::noop();
        store
            .register_namespace(
                &spec,
                Arc::new(JsonValuesMapping::new(
                    crate::props::accounts::namespace_name(),
                )),
            )
            .expect("register stand-in accounts");
        let rt = Arc::new(Runtime::new(router.service(), spec, store.clone()));
        router
            .register(rt.clone())
            .expect("router register stand-in");
        for email in emails {
            let mut m = BTreeMap::new();
            m.insert("email".to_string(), PropValue::String((*email).to_string()));
            rt.set(
                RecordKey::collection(
                    crate::props::accounts::namespace_name(),
                    (*email).to_string(),
                ),
                PropValue::Object(m),
                SetOpts {
                    expected_version: Some(Version::zero()),
                    merge: MergeMode::Replace,
                    actor: Actor::service("test").expect("valid actor"),
                    cause: None,
                    ts_ms: 0,
                },
            )
            .await
            .expect("seed stand-in account");
        }
        rt
    }

    /// Hooks over a fresh store with no seeded accounts. The shape/canon/
    /// self-alias rejections fire before any account read, so an empty
    /// accounts table is fine for those.
    async fn empty_hooks() -> AliasesHooks {
        let conn = Connection::open_in_memory().expect("sqlite");
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")
            .expect("pragmas");
        let store = Arc::new(SqliteStore::new("maild", conn).expect("store"));
        let mut router = PropsRouter::new("maild");
        let accounts_rt = standin_accounts(&store, &mut router, &[]).await;
        AliasesHooks::new(accounts_rt)
    }

    #[tokio::test]
    async fn before_set_rejects_bad_address_key() {
        let h = empty_hooks().await;
        let c = ctx("not-an-email", Some(&body("admin@example.com")));
        assert!(h.before_set(&c).await.is_err());
    }

    #[test]
    fn validate_email_shape_reserves_opaque_vtoken_namespace() {
        // The opaque vtoken namespace is reserved — an alias address with a
        // 6/10-char Crockford-base32 local-part is rejected (C9).
        assert!(AliasesHooks::validate_email_shape("alias address", "k4m9p2@example.org").is_err());
        assert!(
            AliasesHooks::validate_email_shape("alias address", "k4m9p2x7qe@example.org").is_err()
        );
        assert!(AliasesHooks::validate_email_shape("alias address", "sales@example.org").is_ok());
    }

    #[tokio::test]
    async fn before_set_rejects_non_canonical_key() {
        let h = empty_hooks().await;
        let c = ctx("MC@example.com", Some(&body("admin@example.com")));
        let e = format!("{}", h.before_set(&c).await.unwrap_err());
        assert!(e.contains("not canonical"), "got: {e}");
    }

    #[tokio::test]
    async fn before_set_rejects_non_canonical_target() {
        let h = empty_hooks().await;
        let c = ctx("mc@example.com", Some(&body("Admin@example.com")));
        let e = format!("{}", h.before_set(&c).await.unwrap_err());
        assert!(e.contains("not canonical"), "got: {e}");
    }

    #[tokio::test]
    async fn before_set_rejects_self_alias() {
        let h = empty_hooks().await;
        let c = ctx("mc@example.com", Some(&body("mc@example.com")));
        let e = format!("{}", h.before_set(&c).await.unwrap_err());
        assert!(e.contains("self-alias"), "got: {e}");
    }

    #[tokio::test]
    async fn before_set_rejects_missing_target() {
        let h = empty_hooks().await;
        let c = ctx(
            "mc@example.com",
            Some(&[("enabled", PropValue::Bool(true))]),
        );
        assert!(h.before_set(&c).await.is_err());
    }

    // ── integration: registered namespace + stand-in accounts ──────────
    /// Returns the aliases runtime over a store whose `maild.accounts`
    /// stand-in is seeded with `accounts` (the valid alias targets).
    async fn fixture_with(accounts: &[&str]) -> Arc<Runtime> {
        let conn = Connection::open_in_memory().expect("sqlite");
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")
            .expect("pragmas");
        let store = Arc::new(SqliteStore::new("maild", conn).expect("store"));
        let mut router = PropsRouter::new("maild");
        let accounts_rt = standin_accounts(&store, &mut router, accounts).await;
        register(&mut router, &store, accounts_rt).expect("register aliases")
    }

    /// The common case: `admin@example.com` is the only seeded account.
    async fn fixture() -> Arc<Runtime> {
        fixture_with(&["admin@example.com"]).await
    }

    /// Like `fixture_with` but also returns the store + accounts runtime
    /// so a test can seed a raced dual-state (the stand-in accounts ns
    /// has noop hooks, so it accepts a colliding row `before_set` would
    /// reject in production).
    async fn fixture_full(accounts: &[&str]) -> (Arc<Runtime>, Arc<Runtime>) {
        let conn = Connection::open_in_memory().expect("sqlite");
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")
            .expect("pragmas");
        let store = Arc::new(SqliteStore::new("maild", conn).expect("store"));
        let mut router = PropsRouter::new("maild");
        let accounts_rt = standin_accounts(&store, &mut router, accounts).await;
        let aliases_rt = register(&mut router, &store, accounts_rt.clone()).expect("register");
        (aliases_rt, accounts_rt)
    }

    async fn seed_standin_account(accounts_rt: &Arc<Runtime>, email: &str) {
        let mut m = BTreeMap::new();
        m.insert("email".to_string(), PropValue::String(email.to_string()));
        accounts_rt
            .set(
                RecordKey::collection(crate::props::accounts::namespace_name(), email.to_string()),
                PropValue::Object(m),
                SetOpts {
                    expected_version: Some(Version::zero()),
                    merge: MergeMode::Replace,
                    actor: Actor::service("test").expect("valid actor"),
                    cause: None,
                    ts_ms: 0,
                },
            )
            .await
            .expect("seed standin account");
    }

    #[tokio::test]
    async fn reconcile_diagnostic_flags_a_raced_shadow() {
        let (aliases_rt, accounts_rt) = fixture_full(&["admin@example.com"]).await;
        set_alias(&aliases_rt, "mc@example.com", "admin@example.com", true)
            .await
            .expect("clean alias");
        // Clean to start.
        let (shadows, dangling) = compute_alias_conflicts(&aliases_rt, &accounts_rt)
            .await
            .expect("compute");
        assert!(shadows.is_empty() && dangling.is_empty(), "clean state");

        // Simulate the raced dual-state the before_set guards normally
        // prevent: make `mc@` ALSO an account (the stand-in's noop hooks
        // accept the colliding row a real `accounts.before_set` would
        // reject) → `mc@` is now a SHADOW the diagnostic must surface.
        seed_standin_account(&accounts_rt, "mc@example.com").await;
        let (shadows, dangling) = compute_alias_conflicts(&aliases_rt, &accounts_rt)
            .await
            .expect("compute after shadow");
        assert_eq!(
            shadows,
            vec!["mc@example.com".to_string()],
            "shadow detected"
        );
        assert!(dangling.is_empty(), "target admin@ is still a real account");
    }

    async fn set_alias(
        rt: &Arc<Runtime>,
        addr: &str,
        target: &str,
        enabled: bool,
    ) -> anyhow::Result<()> {
        let mut m = BTreeMap::new();
        m.insert("address".to_string(), PropValue::String(addr.to_string()));
        m.insert("target".to_string(), PropValue::String(target.to_string()));
        m.insert("enabled".to_string(), PropValue::Bool(enabled));
        rt.set(
            RecordKey::collection(namespace_name(), addr.to_string()),
            PropValue::Object(m),
            SetOpts {
                expected_version: Some(Version::zero()),
                merge: MergeMode::Replace,
                actor: Actor::service("test").expect("valid actor"),
                cause: None,
                ts_ms: 0,
            },
        )
        .await
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("{e}"))
    }

    #[tokio::test]
    async fn resolve_and_sender_authz_round_trip() {
        let rt = fixture().await;
        set_alias(&rt, "mc@example.com", "admin@example.com", true)
            .await
            .expect("set alias");

        // Inbound resolve, case-insensitive on the alias key.
        assert_eq!(
            resolve_alias(&rt, "mc@example.com")
                .await
                .unwrap()
                .as_deref(),
            Some("admin@example.com")
        );
        assert_eq!(
            resolve_alias(&rt, "MC@EXAMPLE.COM")
                .await
                .unwrap()
                .as_deref(),
            Some("admin@example.com"),
            "mixed-case alias resolves identically"
        );
        // resolve_recipient passes a non-alias through UNCHANGED (case kept).
        assert_eq!(
            resolve_recipient(&rt, "Other@example.com").await.unwrap(),
            "Other@example.com"
        );
        assert_eq!(
            resolve_recipient(&rt, "mc@example.com").await.unwrap(),
            "admin@example.com"
        );

        // Outbound: admin@ may send as itself and as its alias mc@; a
        // foreign / unrelated address may not.
        assert!(sender_authorized(&rt, "admin@example.com", "admin@example.com").await);
        assert!(sender_authorized(&rt, "MC@example.com", "admin@example.com").await);
        assert!(!sender_authorized(&rt, "mc@example.com", "someone@example.com").await);
        assert!(!sender_authorized(&rt, "evil@example.com", "admin@example.com").await);
    }

    #[tokio::test]
    async fn disabled_alias_resolves_to_nothing() {
        let rt = fixture().await;
        set_alias(&rt, "mc@example.com", "admin@example.com", false)
            .await
            .expect("set disabled alias");
        assert_eq!(resolve_alias(&rt, "mc@example.com").await.unwrap(), None);
        // …and an authenticated account can't borrow a disabled alias.
        assert!(!sender_authorized(&rt, "mc@example.com", "admin@example.com").await);
    }

    #[tokio::test]
    async fn account_alias_disjointness_enforced() {
        // admin@ + boss@ are accounts; mc@ / sales@ / ghost@ are not.
        let rt = fixture_with(&["admin@example.com", "boss@example.com"]).await;

        // OK: a non-account key pointing at a real account.
        set_alias(&rt, "mc@example.com", "admin@example.com", true)
            .await
            .expect("alias to a real account");
        // A second independent alias to the same real account is fine.
        set_alias(&rt, "sales@example.com", "admin@example.com", true)
            .await
            .expect("independent alias to same target");

        // REJECT: target is not a real account (would deliver into a void).
        let dead = set_alias(&rt, "x@example.com", "ghost@example.com", true)
            .await
            .unwrap_err();
        assert!(
            format!("{dead}").contains("not a maild.accounts row"),
            "got: {dead}"
        );

        // REJECT: alias KEY is itself a real account (would shadow it).
        let shadow = set_alias(&rt, "admin@example.com", "boss@example.com", true)
            .await
            .unwrap_err();
        assert!(
            format!("{shadow}").contains("already a real account"),
            "got: {shadow}"
        );

        // No chaining is possible by construction: a target is always an
        // account and an account is never an alias key, so `mc@` (a key)
        // can never appear as another alias's target — `x@ -> mc@` is
        // rejected for the same "not a maild.accounts row" reason.
        let chain = set_alias(&rt, "x@example.com", "mc@example.com", true)
            .await
            .unwrap_err();
        assert!(
            format!("{chain}").contains("not a maild.accounts row"),
            "got: {chain}"
        );
    }
}
