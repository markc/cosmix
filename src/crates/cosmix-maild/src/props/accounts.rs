//! SPEC 12 property-substrate adoption for `maild.accounts`.
//!
//! Wires the existing `accounts` SQL table (see [`crate::db`]) into a
//! Saga-lifecycle property namespace served by `maild.props.*`. The
//! substrate owns metadata (`__props_records`, `__props_events`,
//! `__props_audit_keys`); this module bridges the daemon's business
//! columns through [`AccountsMapping`] and runs the post-commit
//! mailbox / calendar / addressbook seeding through
//! [`AccountsHooks::after_set`] — the same flow `cosmix-maild account
//! add` runs on the CLI, modulo idempotency for Saga replay.
//!
//! ## Round-trip contract
//!
//! `commit_set` in [`cosmix_props::sqlite`] computes the final value
//! from the inbound `PropValue` (Replace) or merge-with-prior (Patch)
//! and hands that value to [`AccountsMapping::upsert`]. The same value
//! is reflected back to subscribers as the record's `value` projection.
//! The mapping therefore MUST round-trip cleanly — applying any
//! transform inside `upsert` (e.g. hashing the password) would let a
//! follow-up `merge: patch` operate on transformed bytes and corrupt
//! the field.
//!
//! Practical consequence: callers MUST pre-hash the password before
//! calling `maild.props.set`. The CLI path (`cosmix-maild account add`)
//! already does this via `bcrypt::hash`; the Bus/WG-mesh equivalent
//! must do the same. Phase 1 accepts the supplied bytes verbatim.
//!
//! ## Auth policy (Phase 1)
//!
//! Permissive per SPEC 09: any peer reaching the dispatcher receives
//! the four `props.{read,write,describe,audit}:maild.accounts`
//! capabilities. The WireGuard `/24` is the trust domain; broker-side
//! reservation (SPEC 12 §15.5, landed in C6) gates the records.changed
//! and audit topics so a peer cannot bypass the verbs to spoof events.
//! Phase 2+ will narrow this to operator vs service capability tiers.

use std::sync::{Arc, Mutex, OnceLock};

use anyhow::Result;
use cosmix_mds::Mds;
use cosmix_props::bus::mutation::PropsRouter;
use cosmix_props::capability::{Capability, CapabilitySet};
use cosmix_props::hooks::{HookCtx, HookError, HookFuture, HookHandler, Hooks};
use cosmix_props::namespace::{
    AuthPolicy, Cardinality, FieldSchema, FieldType, NamespaceLifecycle, NamespaceName,
    NamespaceSpec, PeerIdentity, PropertySchema, StorageBackendKind,
};
use cosmix_props::record::{Actor as RecordActor, RecordKey};
use cosmix_props::runtime::{DeleteOpts, Runtime, RuntimeError};
use cosmix_props::sqlite::{SqliteStore, SqliteTableMapping};
use cosmix_props::store::StoreError;
use cosmix_props::value::PropValue;
use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

use crate::mailstore::{self, MailStore, SqliteMailStore};

/// Standard JMAP default mailboxes, in deploy/display order. Each
/// tuple is `(display name, role attribute, sortOrder)`. Owned by
/// this module so the substrate's `after_set` seeder and the
/// `maild.accounts.seed_mailboxes` Bus action share one definition.
pub(crate) const DEFAULT_MAILBOXES: [(&str, mailstore::MailboxRole, u32); 6] = [
    ("Inbox", mailstore::MailboxRole::Inbox, 10),
    ("Drafts", mailstore::MailboxRole::Drafts, 20),
    ("Sent", mailstore::MailboxRole::Sent, 30),
    ("Junk", mailstore::MailboxRole::Junk, 40),
    ("Trash", mailstore::MailboxRole::Trash, 50),
    ("Archive", mailstore::MailboxRole::Archive, 60),
];

/// Unqualified namespace name. Fully qualified form (`maild.accounts`)
/// is library-derived; this constant is the wire `namespace:` header.
pub const NAMESPACE: &str = "accounts";

/// Build the per-namespace [`NamespaceName`]. Infallible: the literal
/// "accounts" satisfies `[a-z][a-z0-9_]*`.
pub fn namespace_name() -> NamespaceName {
    NamespaceName::new(NAMESPACE).expect("constant namespace name is valid")
}

/// SPEC 12 §4.3 schema for the `accounts` namespace. Mirrors the
/// columns of `db::SCHEMA`'s accounts table; `id` is intentionally
/// absent (the substrate uses `email` as the wire `key:` per the
/// namespace's `Cardinality::Collection { primary_key_field: "email" }`).
pub fn schema() -> PropertySchema {
    PropertySchema::new(vec![
        FieldSchema {
            name: "email".into(),
            ty: FieldType::Email,
            default: None,
            secret: false,
            help: "Account email address (primary key)".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "password".into(),
            ty: FieldType::String,
            default: None,
            // SPEC 12 §4.3 secret-field marker. records.changed
            // payload is `pointer` per default (see [`spec`] below),
            // so the secret value is never broadcast inline; audit
            // rows redact this field's bytes when computing the
            // public field-change list.
            secret: true,
            help: "Pre-hashed password bytes (caller supplies bcrypt output)".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "name".into(),
            ty: FieldType::Option {
                inner: Box::new(FieldType::String),
            },
            default: Some(PropValue::Null),
            secret: false,
            help: "Display name".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "quota".into(),
            ty: FieldType::I64,
            default: Some(PropValue::Int(0)),
            secret: false,
            help: "Per-account storage quota in bytes (0 = unlimited)".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "spam_enabled".into(),
            ty: FieldType::Bool,
            default: Some(PropValue::Bool(true)),
            secret: false,
            help: "Whether to run inbound mail through the Bayesian classifier".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "spam_threshold".into(),
            ty: FieldType::F64,
            default: Some(PropValue::Float(0.5)),
            secret: false,
            help: "Spam score >= this routes to Junk (0.0..1.0)".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "mfa_enabled".into(),
            ty: FieldType::Bool,
            default: Some(PropValue::Bool(false)),
            secret: false,
            help: "Whether login requires an emailed second-factor code (email-2FA, P3)".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
    ])
}

/// [`SqliteTableMapping`] over the existing `accounts` table. The
/// substrate calls all four methods inside its own `BEGIN IMMEDIATE`
/// commit transaction; we MUST NOT open nested transactions or commit
/// here.
///
/// `bootstrap` is a no-op because [`crate::db::Db::migrate`] already
/// runs the canonical schema before the substrate is wired up; the
/// substrate's idempotent re-call therefore has nothing to do.
pub struct AccountsMapping;

impl SqliteTableMapping for AccountsMapping {
    fn bootstrap(&self, _conn: &Connection) -> rusqlite::Result<()> {
        Ok(())
    }

    fn read(&self, conn: &Connection, key: &str) -> rusqlite::Result<Option<PropValue>> {
        let row = conn
            .query_row(
                "SELECT email, password, name, quota, \
                 COALESCE(spam_enabled, 1) AS spam_enabled, \
                 COALESCE(spam_threshold, 0.5) AS spam_threshold, \
                 COALESCE(mfa_enabled, 0) AS mfa_enabled \
                 FROM accounts WHERE email = ?1",
                params![key],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, i32>(4)? != 0,
                        r.get::<_, f64>(5)?,
                        r.get::<_, i32>(6)? != 0,
                    ))
                },
            )
            .optional()?;
        Ok(row.map(account_row_to_value))
    }

    fn list(&self, conn: &Connection) -> rusqlite::Result<Vec<(String, PropValue)>> {
        let mut stmt = conn.prepare(
            "SELECT email, password, name, quota, \
             COALESCE(spam_enabled, 1) AS spam_enabled, \
             COALESCE(spam_threshold, 0.5) AS spam_threshold, \
             COALESCE(mfa_enabled, 0) AS mfa_enabled \
             FROM accounts ORDER BY email",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i32>(4)? != 0,
                r.get::<_, f64>(5)?,
                r.get::<_, i32>(6)? != 0,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let tup = row?;
            let key = tup.0.clone();
            out.push((key, account_row_to_value(tup)));
        }
        Ok(out)
    }

    fn upsert(&self, conn: &Connection, key: &str, value: &PropValue) -> rusqlite::Result<()> {
        let fields = value_to_account_fields(key, value).map_err(|msg| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::<dyn std::error::Error + Send + Sync>::from(msg),
            )
        })?;
        // SPEC 12 §4.4 — same record under collection primary key
        // `email` is upserted. Existing `id` is preserved by SQLite's
        // ON CONFLICT update (we do not touch the id column). New
        // rows allocate a fresh AUTOINCREMENT id.
        conn.execute(
            "INSERT INTO accounts \
             (email, password, name, quota, spam_enabled, spam_threshold, mfa_enabled) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT(email) DO UPDATE SET \
                password = excluded.password, \
                name = excluded.name, \
                quota = excluded.quota, \
                spam_enabled = excluded.spam_enabled, \
                spam_threshold = excluded.spam_threshold, \
                mfa_enabled = excluded.mfa_enabled",
            params![
                fields.email,
                fields.password,
                fields.name,
                fields.quota,
                fields.spam_enabled as i32,
                fields.spam_threshold,
                fields.mfa_enabled as i32,
            ],
        )?;
        // NOTE: the positive Basic-credential cache is invalidated POST-commit in
        // `AccountsHooks::after_set`, NOT here. Clearing inside this BEGIN IMMEDIATE
        // txn would race a concurrent Basic verify on the auth connection: in WAL it
        // still reads the old (committed) row, bcrypt-passes, and re-caches the stale
        // credential between this clear and the commit. Post-commit is the only point
        // where a fresh verify is guaranteed to read the new row.
        Ok(())
    }

    fn delete(&self, conn: &Connection, key: &str) -> rusqlite::Result<()> {
        // ON DELETE CASCADE on dependent tables (mailboxes /
        // threads / blobs / emails / changelog / calendars /
        // addressbooks / submissions / vacation_responses) drops
        // SQL-side per-account state. MDS-side per-account
        // containers are NOT dropped here — that is a `purge_account_set`
        // surface deferred per the C7 plan ("after_delete best-effort
        // MDS purge hook (deferred if it expands scope)").
        conn.execute("DELETE FROM accounts WHERE email = ?1", params![key])?;
        // Basic-credential cache invalidation happens POST-commit in
        // `AccountsHooks::after_delete` — see the upsert note for the WAL race that
        // makes an in-txn clear here ineffective.
        Ok(())
    }
}

struct AccountFields {
    email: String,
    password: String,
    name: Option<String>,
    quota: i64,
    spam_enabled: bool,
    spam_threshold: f64,
    mfa_enabled: bool,
}

fn account_row_to_value(
    (email, password, name, quota, spam_enabled, spam_threshold, mfa_enabled): (
        String,
        String,
        Option<String>,
        i64,
        bool,
        f64,
        bool,
    ),
) -> PropValue {
    let mut m = std::collections::BTreeMap::new();
    m.insert("email".into(), PropValue::String(email));
    m.insert("password".into(), PropValue::String(password));
    m.insert(
        "name".into(),
        match name {
            Some(s) => PropValue::String(s),
            None => PropValue::Null,
        },
    );
    m.insert("quota".into(), PropValue::Int(quota));
    m.insert("spam_enabled".into(), PropValue::Bool(spam_enabled));
    m.insert("spam_threshold".into(), PropValue::Float(spam_threshold));
    m.insert("mfa_enabled".into(), PropValue::Bool(mfa_enabled));
    PropValue::Object(m)
}

fn value_to_account_fields(key: &str, value: &PropValue) -> Result<AccountFields, String> {
    let obj = value
        .as_object()
        .ok_or_else(|| "accounts record must be a JSON object".to_string())?;
    let email = match obj.get("email") {
        Some(PropValue::String(s)) => s.clone(),
        Some(_) => return Err("accounts.email must be a string".into()),
        // `before_set` rejects this branch before we get here; the
        // guard is duplicated so a future caller invoking the
        // mapping outside the runtime doesn't insert nameless rows.
        None => return Err("accounts.email is required".into()),
    };
    if email != key {
        // The runtime's `validate_primary_key` (SPEC 12 §6.2) already
        // pins this for create/replace; a Patch may omit `email` from
        // the body, in which case the merged value's `email` MUST
        // equal the wire key. Re-check at the mapping boundary so a
        // future caller cannot smuggle an inconsistency past it.
        return Err(format!(
            "accounts.email {email:?} does not match wire key {key:?}"
        ));
    }
    let password = match obj.get("password") {
        Some(PropValue::String(s)) => s.clone(),
        Some(_) => return Err("accounts.password must be a string".into()),
        None => return Err("accounts.password is required".into()),
    };
    let name = match obj.get("name") {
        Some(PropValue::String(s)) => Some(s.clone()),
        Some(PropValue::Null) | None => None,
        Some(_) => return Err("accounts.name must be a string or null".into()),
    };
    let quota = match obj.get("quota") {
        Some(PropValue::Int(n)) => *n,
        Some(PropValue::UInt(n)) => {
            i64::try_from(*n).map_err(|_| "accounts.quota out of i64 range".to_string())?
        }
        Some(PropValue::Null) | None => 0,
        Some(_) => return Err("accounts.quota must be an integer".into()),
    };
    let spam_enabled = match obj.get("spam_enabled") {
        Some(PropValue::Bool(b)) => *b,
        Some(PropValue::Null) | None => true,
        Some(_) => return Err("accounts.spam_enabled must be a bool".into()),
    };
    let spam_threshold = match obj.get("spam_threshold") {
        Some(PropValue::Float(f)) => *f,
        Some(PropValue::Int(n)) => *n as f64,
        Some(PropValue::Null) | None => 0.5,
        Some(_) => return Err("accounts.spam_threshold must be a number".into()),
    };
    // Email-2FA opt-in (P3). Default false: an account (or a Patch body that
    // omits the field) is non-2FA unless explicitly enabled — fail-safe, so a
    // partial write can never silently turn the second factor OFF for an
    // already-enrolled account only when the field is genuinely absent; a
    // Patch merge re-supplies the stored value before reaching here.
    let mfa_enabled = match obj.get("mfa_enabled") {
        Some(PropValue::Bool(b)) => *b,
        Some(PropValue::Null) | None => false,
        Some(_) => return Err("accounts.mfa_enabled must be a bool".into()),
    };
    Ok(AccountFields {
        email,
        password,
        name,
        quota,
        spam_enabled,
        spam_threshold,
        mfa_enabled,
    })
}

/// SPEC 12 §6.5 hook handler for the `accounts` namespace.
///
/// `before_set`: validates the email shape (cheap, no IO). The
/// substrate's primary-key check (`validate_primary_key`) runs first
/// and pins `email == key` for Replace; this hook adds the per-field
/// format check that `FieldType::Email` documents but does not enforce
/// in v0.1.
///
/// `after_set`: post-commit MDS bootstrap. Looks up the freshly-
/// allocated `accounts.id` via a separate connection (the substrate's
/// commit has already released its transaction by this point, and
/// WAL guarantees a new reader sees the committed row) and seeds the
/// six default mailboxes plus a default calendar and addressbook.
/// Idempotent against Saga replay — re-running on a record that
/// already passed `after_set` returns `Ok(())` without duplicating
/// state.
pub struct AccountsHooks {
    /// Maild's existing rusqlite connection — same DB file as the
    /// substrate's store, opened independently. Used for post-commit
    /// account-id lookup. Wrapped in `Arc<Mutex<_>>` so the hook can
    /// be shared across the runtime + the existing CLI/JMAP paths.
    db_conn: Arc<Mutex<Connection>>,
    mailstore: Arc<SqliteMailStore>,
    /// Runtime handle for `maild.account_overrides`. Held directly
    /// (not behind a `OnceLock`) because the registration sequence
    /// in `crate::main` builds account_overrides first, so the
    /// overrides runtime exists by the time this struct is
    /// constructed. Used for two things:
    /// - `before_set` (create branch): reject orphan adoption when a
    ///   stale override row survives a previous cascade failure.
    /// - `after_delete`: cascade-delete the override row keyed by the
    ///   same email.
    overrides_runtime: Arc<Runtime>,
    /// Runtime handle for `maild.aliases`, behind a `OnceLock` because
    /// aliases is registered AFTER accounts (so the runtime can't be
    /// passed at construction). Read in `before_set`'s create branch to
    /// reject an account email that already exists as an alias key — the
    /// reciprocal of the aliases-side "an alias key must not be a real
    /// account" guard, keeping the account/alias-key sets disjoint.
    aliases_runtime: Arc<OnceLock<Arc<Runtime>>>,
}

impl AccountsHooks {
    pub fn new(
        db_conn: Arc<Mutex<Connection>>,
        mailstore: Arc<SqliteMailStore>,
        overrides_runtime: Arc<Runtime>,
        aliases_runtime: Arc<OnceLock<Arc<Runtime>>>,
    ) -> Self {
        Self {
            db_conn,
            mailstore,
            overrides_runtime,
            aliases_runtime,
        }
    }

    /// Best-effort RFC-5321 shape check. Pragmatic: substrate
    /// `FieldType::Email` ships with no semantic check today; we
    /// reject obvious garbage (missing `@`, empty local/domain,
    /// whitespace) so a typo doesn't sail through to the SQL
    /// `UNIQUE(email)` constraint and surface as `StorageError`.
    fn validate_email_shape(s: &str) -> Result<(), HookError> {
        if s.is_empty() {
            return Err(HookError::validation("email is empty"));
        }
        if s.chars().any(char::is_whitespace) {
            return Err(HookError::validation("email contains whitespace"));
        }
        let mut parts = s.split('@');
        let local = parts.next().unwrap_or("");
        let domain = parts.next().unwrap_or("");
        if parts.next().is_some() {
            return Err(HookError::validation("email has multiple @ separators"));
        }
        if local.is_empty() || domain.is_empty() {
            return Err(HookError::validation(
                "email must have non-empty local and domain parts",
            ));
        }
        if !domain.contains('.') {
            return Err(HookError::validation("email domain must contain a dot"));
        }
        // Reserve the opaque vtoken namespace (C9): a 6/10-char Crockford-base32
        // local-part is a capability-token shape, NOT a mailbox name — otherwise a
        // real mailbox could later collide with the reserved opaque namespace and
        // be routed through the size-capped async token path.
        if crate::vtoken::is_opaque_shaped(local) {
            return Err(HookError::validation(
                "email local-part matches the reserved opaque vtoken shape (6/10 Crockford-base32)",
            ));
        }
        Ok(())
    }
}

impl HookHandler for AccountsHooks {
    fn before_set<'a>(&'a self, ctx: &'a HookCtx) -> HookFuture<'a, ()> {
        Box::pin(async move {
            // Wire key is the canonical email (substrate enforces
            // `key == value.email` for Replace). Validate the key
            // string so a Patch that omits `email` from the body
            // still rejects malformed wire keys.
            Self::validate_email_shape(&ctx.key.key)?;
            // If the body carries `email`, double-check it too —
            // catches the case where the runtime accepted a Patch
            // body the schema-level check would have rejected.
            if let Some(PropValue::Object(m)) = &ctx.new
                && let Some(PropValue::String(s)) = m.get("email")
            {
                Self::validate_email_shape(s)?;
            }

            // Orphan-adoption detection. Only the create branch
            // (`ctx.old.is_none()`) needs to check; a Patch against
            // an existing row cannot adopt an orphan by definition.
            // The check guards against two ways an override row can
            // outlive its parent account: (1) `after_delete` cascade
            // failed and left a stale row behind, (2) a TOCTOU race
            // between `account_overrides.set` and `accounts.delete`.
            // Both produce the same orphan class and both are caught
            // here at next-create time.
            //
            // The message prefix `orphan_override_detected:` is the
            // CLI's parse handle (Phase 1's wire vocabulary has no
            // typed `error_code` slot for daemon-specific codes;
            // operators consume the prefix on the rejection body).
            if ctx.old.is_none() {
                let overrides_ns = crate::props::account_overrides::namespace_name();
                let overrides_key = RecordKey::collection(overrides_ns, ctx.key.key.clone());
                match self.overrides_runtime.store().get(&overrides_key).await {
                    Ok(_) => {
                        return Err(HookError::validation(format!(
                            "{} stale account_overrides row for {:?}; run \
                             `cosmix-maild account-overrides clear --email <addr>` \
                             before re-creating this account",
                            crate::props::account_overrides::ORPHAN_OVERRIDE_PREFIX,
                            ctx.key.key,
                        )));
                    }
                    Err(StoreError::NotFound) => {
                        // Clean slate — allow create.
                    }
                    Err(e) => {
                        return Err(HookError::hook(format!(
                            "account_overrides read on orphan check: {e}"
                        )));
                    }
                }

                // Reciprocal account/alias-key disjointness: refuse to
                // create an account whose email is already an alias key,
                // or `maild.aliases` would shadow the new account (mail
                // to it would resolve to the alias target). The aliases
                // runtime is populated at startup before serving, so the
                // lock is always set when an operator creates an account;
                // if it is somehow empty we skip rather than block create
                // (the impossible startup-window case).
                if let Some(aliases_rt) = self.aliases_runtime.get() {
                    let aliases_key = RecordKey::collection(
                        crate::props::aliases::namespace_name(),
                        ctx.key.key.clone(),
                    );
                    match aliases_rt.store().get(&aliases_key).await {
                        Ok(_) => {
                            return Err(HookError::validation(format!(
                                "address {:?} is already a maild.aliases alias — remove the \
                                 alias before creating an account with this address (an \
                                 address cannot be both an account and an alias)",
                                ctx.key.key
                            )));
                        }
                        Err(StoreError::NotFound) => {}
                        Err(e) => {
                            return Err(HookError::hook(format!(
                                "maild.aliases read on account-create collision check: {e}"
                            )));
                        }
                    }
                }
            }
            Ok(())
        })
    }

    fn after_delete<'a>(&'a self, ctx: &'a HookCtx) -> HookFuture<'a, ()> {
        Box::pin(async move {
            // POST-commit: the account row is gone, so any positive Basic-credential
            // cache entry for it is now stale — drop the whole cache so the removed
            // credential stops authenticating on Basic surfaces at once (rather than
            // lingering up to the 30s cache TTL). Cheap; account deletes are rare.
            crate::auth::basic::clear_verify_cache();
            let email = ctx.key.key.clone();
            let overrides_ns = crate::props::account_overrides::namespace_name();
            let key = RecordKey::collection(overrides_ns, email.clone());
            let opts = DeleteOpts {
                expected_version: None,
                // Phase 1 `Actor` API has no Daemon { source } variant;
                // attribute via `service("maild")` plus the cascade
                // origin in `cause` so the audit stream shows the
                // hop. See SPEC 12 §5.5 / §10.
                actor: RecordActor::service("maild"),
                cause: Some("maild.accounts.after_delete:cascade".into()),
                ts_ms: 0,
            };
            match self.overrides_runtime.delete(key, opts).await {
                Ok(_) => Ok(()),
                // `Runtime::delete` reads first to populate the
                // delete-hook context (runtime.rs:369); a missing
                // override row surfaces as
                // `RuntimeError::Store(StoreError::NotFound)`.
                // Accounts without overrides hit this branch on
                // every account delete — treat as success.
                Err(RuntimeError::Store(StoreError::NotFound)) => Ok(()),
                Err(e) => {
                    tracing::error!(
                        target: "cosmix_maild::props::accounts::cascade",
                        email = %email, error = %e,
                        "account_overrides cascade-delete failed; row may be orphaned. \
                         Operator: run `cosmix-maild account-overrides clear --email <addr>` \
                         before re-creating this account."
                    );
                    // Returning Ok here is deliberate: the parent
                    // account delete already committed and we MUST
                    // NOT bubble a hook error that re-enters the
                    // runtime. The next account create for this
                    // email is rejected by the orphan-detection
                    // check above, which is the operator's signal.
                    Ok(())
                }
            }
        })
    }

    fn after_set<'a>(&'a self, ctx: &'a HookCtx) -> HookFuture<'a, ()> {
        Box::pin(async move {
            // POST-commit: a create or a password change (ON CONFLICT … UPDATE
            // password) committed before this hook fires, so a fresh Basic verify now
            // reads the new row. Drop the whole positive cache so a rotated password
            // takes effect on Basic surfaces at once instead of lingering up to the
            // 30s cache TTL. Unconditional + first, before the MDS-bootstrap logic
            // below can early-return.
            crate::auth::basic::clear_verify_cache();
            let email = ctx.key.key.clone();
            let db_conn = self.db_conn.clone();
            let mailstore = self.mailstore.clone();

            // Resolve account_id via maild's connection. WAL +
            // foreign_keys ON; the substrate's INSERT committed
            // before this hook fires, so a fresh transaction here
            // sees the row.
            let id = tokio::task::spawn_blocking(move || -> Result<i32, HookError> {
                let conn = db_conn
                    .lock()
                    .map_err(|e| HookError::hook(format!("db lock poisoned: {e}")))?;
                conn.query_row(
                    "SELECT id FROM accounts WHERE email = ?1",
                    params![email],
                    |r| r.get::<_, i32>(0),
                )
                .map_err(|e| HookError::hook(format!("account lookup after commit: {e}")))
            })
            .await
            .map_err(|e| HookError::hook(format!("join error: {e}")))??;

            // MDS-side seeding. `tokio::task::spawn_blocking` so the
            // synchronous mailstore + rusqlite calls do not block
            // the runtime worker. Idempotent under §6.5's "runtime
            // does NOT serialise per-key" rule: `ensure_account_set_idempotent`
            // tolerates the create-set TOCTOU race, `seed_default_mailboxes_idempotent`
            // tolerates the create-mailbox TOCTOU race, and
            // `seed_default_personal_collections` uses INSERT OR IGNORE.
            let db_conn = self.db_conn.clone();
            tokio::task::spawn_blocking(move || -> Result<(), HookError> {
                ensure_account_set_idempotent(&mailstore, id)
                    .map_err(|e| HookError::hook(format!("ensure_account_set: {e}")))?;
                seed_default_mailboxes_idempotent(&mailstore, id)
                    .map_err(|e| HookError::hook(format!("seed_default_mailboxes: {e}")))?;
                seed_default_personal_collections(&db_conn, id)
                    .map_err(|e| HookError::hook(format!("seed personal collections: {e}")))?;
                Ok(())
            })
            .await
            .map_err(|e| HookError::hook(format!("join error: {e}")))?
        })
    }
}

/// Wrapper around [`SqliteMailStore::ensure_account_set`] that
/// tolerates the create-set TOCTOU race between two concurrent
/// `after_set` invocations on the same account email. The underlying
/// method lists first, then `create_set`s if missing — two parallel
/// callers can both see "missing" and the loser sees
/// `cosmix_mds::Error::SetAlreadyExists` from `cosmix-mds`. Per SPEC
/// 12 §6.5 the runtime does NOT serialise per-key, so this race is
/// real and must be absorbed here.
fn ensure_account_set_idempotent(mailstore: &SqliteMailStore, account_id: i32) -> Result<()> {
    match mailstore.ensure_account_set(account_id) {
        Ok(_) => Ok(()),
        Err(e)
            if matches!(
                e.downcast_ref::<cosmix_mds::Error>(),
                Some(cosmix_mds::Error::SetAlreadyExists { .. })
            ) =>
        {
            // Re-verify the set is actually present before swallowing.
            // The typed variant above is precise, but a stale cache /
            // disk-state desync would still mean the create did not in
            // fact land — fall through to surfacing the error.
            let set = mailstore::account_id_to_setid(account_id);
            let sets = mailstore
                .mds()
                .list_sets()
                .map_err(|le| anyhow::anyhow!("list_sets after collision: {le}"))?;
            if sets.contains(&set) { Ok(()) } else { Err(e) }
        }
        Err(e) => Err(e),
    }
}

/// Idempotent variant of [`crate::seed_default_mailboxes`] — runs
/// from inside the substrate's `after_set` hook (which is called
/// from `tokio::task::spawn_blocking`, so we cannot reach back into
/// async code). Behaviour matches the CLI seed: skip existing roles,
/// reject root-level name collisions without role.
///
/// **Concurrency note.** SPEC 12 §6.5 / `runtime.rs` documents that the
/// runtime does NOT serialise `set → after_set → complete` per key — two
/// concurrent `props.set` calls against the same email both reach this
/// path. To stay idempotent under that race we (a) re-list after a
/// `"mailbox already exists"` collision and treat it as success if the
/// expected role is now present, and (b) treat `ensure_account_set`'s
/// `"already exists"` shape the same way upstream. Without this the
/// loser of the race surfaces a spurious Saga `Failed`, leaving the
/// row Provisioning until §11 reconciliation lands.
pub(crate) fn seed_default_mailboxes_idempotent(
    mailstore: &SqliteMailStore,
    account_id: i32,
) -> Result<()> {
    let mut existing = mailstore.list_mailboxes(account_id)?;
    for (mbox_name, role, sort_order) in DEFAULT_MAILBOXES {
        if existing.iter().any(|m| m.role == Some(role)) {
            continue;
        }
        if let Some(conflict) = existing
            .iter()
            .find(|m| m.parent.is_none() && m.name == mbox_name && m.role.is_none())
        {
            return Err(anyhow::anyhow!(
                "cannot seed role {role:?}: mailbox named {mbox_name:?} \
                 (id {:?}) already exists without a role — \
                 reconcile manually before re-running",
                conflict.id
            ));
        }
        let attrs = mailstore::jmap_attrs_for_create(Some(role), sort_order, true);
        match mailstore.create_mailbox(account_id, mbox_name, None, attrs) {
            Ok(_) => {}
            Err(e)
                if {
                    let m = e.to_string();
                    // Two concurrent-seed loser paths:
                    // * "mailbox already exists" — peer won the
                    //   `(parent, name)` race.
                    // * "mailbox special-use already in use" — peer
                    //   won the SPECIAL-USE uniqueness barrier in
                    //   `MailStore::create_mailbox` (added with T6
                    //   storage atomicity). Either way the intended
                    //   role is now present; re-read and verify.
                    m.starts_with("mailbox already exists")
                        || m.starts_with("mailbox special-use already in use")
                } =>
            {
                existing = mailstore.list_mailboxes(account_id)?;
                if !existing.iter().any(|m| m.role == Some(role)) {
                    return Err(anyhow::anyhow!(
                        "mailbox {mbox_name:?} exists without role {role:?} after race; \
                         reconcile manually"
                    ));
                }
                continue;
            }
            Err(e) => return Err(e),
        }
        // Refresh `existing` so the next iteration sees this insert
        // (and the conflict check stays accurate for sibling roles).
        existing = mailstore.list_mailboxes(account_id)?;
    }
    Ok(())
}

/// Idempotently create the flat top-level CONTENT folders (`Posts`,
/// `Pages`) for one account — the vtoken email-to-post destination tree
/// (the framework `seed_content_tree_idempotent`, post-plan Stage 0).
/// Subscribed, no special role; skips a folder already present by name at
/// the top level. Returns the names actually created (empty if all
/// already existed).
pub(crate) fn seed_content_folders_idempotent(
    mailstore: &SqliteMailStore,
    account_id: i32,
) -> Result<Vec<&'static str>> {
    const CONTENT_FOLDERS: &[(&str, u32)] = &[("Posts", 110), ("Pages", 120)];
    let existing = mailstore.list_mailboxes(account_id)?;
    let mut created: Vec<&'static str> = Vec::new();
    for (name, sort_order) in CONTENT_FOLDERS {
        if existing
            .iter()
            .any(|m| m.parent.is_none() && m.name.as_str() == *name)
        {
            continue;
        }
        let attrs = mailstore::jmap_attrs_for_create(None, *sort_order, true);
        match mailstore.create_mailbox(account_id, name, None, attrs) {
            Ok(_) => created.push(*name),
            // Race loser: a peer created the same top-level name first.
            Err(e) if e.to_string().starts_with("mailbox already exists") => {}
            Err(e) => return Err(e),
        }
    }
    Ok(created)
}

/// INSERT OR IGNORE the per-account default calendar + addressbook.
/// Runs from `after_set` so Saga replay finds these rows already
/// present and remains a no-op.
fn seed_default_personal_collections(conn: &Arc<Mutex<Connection>>, account_id: i32) -> Result<()> {
    let conn = conn
        .lock()
        .map_err(|e| anyhow::anyhow!("db lock poisoned: {e}"))?;
    let cal_id = Uuid::new_v4().to_string();
    conn.execute(
        "INSERT OR IGNORE INTO calendars (id, account_id, name, color) VALUES (?1, ?2, ?3, ?4)",
        params![cal_id, account_id, "Personal", "#4285f4"],
    )?;
    let ab_id = Uuid::new_v4().to_string();
    conn.execute(
        "INSERT OR IGNORE INTO addressbooks (id, account_id, name) VALUES (?1, ?2, ?3)",
        params![ab_id, account_id, "Contacts"],
    )?;
    Ok(())
}

/// SPEC 12 §7.2 [`AuthPolicy`] for `maild.accounts`. Phase 1 grants
/// every property capability to every reachable peer; the
/// WireGuard `/24` trust domain (SPEC 09) is what bounds reachability.
/// Phase 2+ will narrow this to operator / service tiers.
pub fn auth_policy() -> AuthPolicy {
    fn resolve(_peer: &PeerIdentity) -> CapabilitySet {
        [
            "props.read:maild.accounts",
            "props.write:maild.accounts",
            "props.describe:maild.accounts:public",
            "props.describe:maild.accounts:full",
            "props.audit:maild.accounts",
        ]
        .into_iter()
        .map(Capability::from)
        .collect()
    }
    AuthPolicy::new(resolve)
}

/// Build the [`NamespaceSpec`]. `hooks` is parameterised so tests can
/// substitute a fake handler; production code calls
/// [`register`] which builds the real [`AccountsHooks`].
pub fn spec(hooks: Hooks) -> NamespaceSpec {
    let mut s = NamespaceSpec::new(
        namespace_name(),
        schema(),
        Cardinality::Collection {
            primary_key_field: "email".into(),
        },
        StorageBackendKind::SqliteTable {
            table: "accounts".into(),
        },
    );
    s.lifecycle = NamespaceLifecycle::Saga;
    s.auth = auth_policy();
    s.hooks = hooks;
    s
}

/// Wire the `maild.accounts` namespace into the substrate. Returns
/// the namespace's `Arc<Runtime>` so the caller can populate the
/// `account_overrides → accounts` cycle-breaking `OnceLock`. Idempotent
/// across multiple calls only at the [`PropsRouter`] level — the
/// underlying [`SqliteStore::register_namespace`] is one-shot per
/// (store, namespace) pair.
pub fn register(
    router: &mut PropsRouter,
    store: &Arc<SqliteStore>,
    mailstore: Arc<SqliteMailStore>,
    db_conn: Arc<Mutex<Connection>>,
    overrides_runtime: Arc<Runtime>,
    aliases_runtime: Arc<OnceLock<Arc<Runtime>>>,
) -> Result<Arc<Runtime>> {
    let hooks = Hooks::new(AccountsHooks::new(
        db_conn,
        mailstore,
        overrides_runtime,
        aliases_runtime,
    ));
    let spec = spec(hooks);
    store
        .register_namespace(&spec, Arc::new(AccountsMapping))
        .map_err(|e| anyhow::anyhow!("register accounts namespace in store: {e}"))?;
    let runtime = Arc::new(Runtime::new(router.service(), spec, store.clone()));
    router
        .register(runtime.clone())
        .map_err(|e| anyhow::anyhow!("register accounts runtime on router: {e}"))?;
    Ok(runtime)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_props::record::{Actor, RecordKey, Version};

    #[test]
    fn namespace_name_is_accounts() {
        assert_eq!(namespace_name().as_str(), "accounts");
    }

    #[test]
    fn schema_has_password_secret_flag() {
        let s = schema();
        let pw = s.field("password").expect("password field present");
        assert!(pw.secret);
        let email = s.field("email").expect("email field present");
        assert!(!email.secret);
    }

    #[test]
    fn auth_policy_grants_phase1_caps() {
        let caps = auth_policy().resolve(&PeerIdentity::default());
        // Five-cap surface; phase 2+ narrows.
        assert!(caps.contains(&Capability::from("props.read:maild.accounts")));
        assert!(caps.contains(&Capability::from("props.write:maild.accounts")));
        assert!(caps.contains(&Capability::from("props.describe:maild.accounts:public")));
        assert!(caps.contains(&Capability::from("props.describe:maild.accounts:full")));
        assert!(caps.contains(&Capability::from("props.audit:maild.accounts")));
        assert!(!caps.contains(&Capability::from("props.write:maild.elsewhere")));
    }

    #[test]
    fn spec_is_saga_collection_keyed_on_email() {
        let s = spec(Hooks::noop());
        assert!(s.is_saga());
        match &s.cardinality {
            Cardinality::Collection { primary_key_field } => {
                assert_eq!(primary_key_field, "email");
            }
            other => panic!("unexpected cardinality: {other:?}"),
        }
        match &s.storage {
            StorageBackendKind::SqliteTable { table } => assert_eq!(table, "accounts"),
            other => panic!("unexpected storage: {other:?}"),
        }
    }

    #[test]
    fn mapping_round_trips_value_through_in_memory_table() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::db::SCHEMA).unwrap();
        let mapping = AccountsMapping;

        let mut m = std::collections::BTreeMap::new();
        m.insert("email".into(), PropValue::String("a@b.c".into()));
        m.insert("password".into(), PropValue::String("hashed".into()));
        m.insert("name".into(), PropValue::String("Alice".into()));
        m.insert("quota".into(), PropValue::Int(42));
        m.insert("spam_enabled".into(), PropValue::Bool(false));
        m.insert("spam_threshold".into(), PropValue::Float(0.75));
        m.insert("mfa_enabled".into(), PropValue::Bool(true));
        let v = PropValue::Object(m);

        mapping.upsert(&conn, "a@b.c", &v).unwrap();
        let back = mapping.read(&conn, "a@b.c").unwrap().expect("row present");
        assert_eq!(back, v);

        // List returns the single row.
        let list = mapping.list(&conn).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].0, "a@b.c");
        assert_eq!(list[0].1, v);

        // Patch-like update: change one field, keep the rest.
        let mut m2 = std::collections::BTreeMap::new();
        m2.insert("email".into(), PropValue::String("a@b.c".into()));
        m2.insert("password".into(), PropValue::String("hashed2".into()));
        m2.insert("name".into(), PropValue::String("Alice".into()));
        m2.insert("quota".into(), PropValue::Int(99));
        m2.insert("spam_enabled".into(), PropValue::Bool(false));
        m2.insert("spam_threshold".into(), PropValue::Float(0.75));
        m2.insert("mfa_enabled".into(), PropValue::Bool(false));
        let v2 = PropValue::Object(m2);
        mapping.upsert(&conn, "a@b.c", &v2).unwrap();
        let back = mapping.read(&conn, "a@b.c").unwrap().unwrap();
        assert_eq!(back, v2);

        mapping.delete(&conn, "a@b.c").unwrap();
        assert!(mapping.read(&conn, "a@b.c").unwrap().is_none());
        assert!(mapping.list(&conn).unwrap().is_empty());
    }

    #[test]
    fn mfa_enabled_defaults_false_and_round_trips() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::db::SCHEMA).unwrap();
        let mapping = AccountsMapping;

        // A create body that OMITS mfa_enabled reads back as false (opt-in default).
        let mut m = std::collections::BTreeMap::new();
        m.insert("email".into(), PropValue::String("a@b.c".into()));
        m.insert("password".into(), PropValue::String("h".into()));
        mapping
            .upsert(&conn, "a@b.c", &PropValue::Object(m))
            .unwrap();
        let back = mapping.read(&conn, "a@b.c").unwrap().expect("row present");
        assert_eq!(
            back.as_object().unwrap().get("mfa_enabled"),
            Some(&PropValue::Bool(false)),
            "omitted mfa_enabled defaults to false (opt-in)"
        );

        // Enabling it survives the round-trip.
        let mut m2 = std::collections::BTreeMap::new();
        m2.insert("email".into(), PropValue::String("a@b.c".into()));
        m2.insert("password".into(), PropValue::String("h".into()));
        m2.insert("mfa_enabled".into(), PropValue::Bool(true));
        mapping
            .upsert(&conn, "a@b.c", &PropValue::Object(m2))
            .unwrap();
        let back = mapping.read(&conn, "a@b.c").unwrap().unwrap();
        assert_eq!(
            back.as_object().unwrap().get("mfa_enabled"),
            Some(&PropValue::Bool(true)),
            "mfa_enabled=true persists"
        );

        // A non-bool value is rejected.
        let mut bad = std::collections::BTreeMap::new();
        bad.insert("email".into(), PropValue::String("a@b.c".into()));
        bad.insert("password".into(), PropValue::String("h".into()));
        bad.insert("mfa_enabled".into(), PropValue::String("yes".into()));
        let err = mapping
            .upsert(&conn, "a@b.c", &PropValue::Object(bad))
            .unwrap_err();
        assert!(err.to_string().contains("mfa_enabled must be a bool"));
    }

    #[test]
    fn mapping_rejects_mismatched_email_key() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::db::SCHEMA).unwrap();
        let mapping = AccountsMapping;
        let mut m = std::collections::BTreeMap::new();
        m.insert("email".into(), PropValue::String("a@b.c".into()));
        m.insert("password".into(), PropValue::String("h".into()));
        let v = PropValue::Object(m);
        let err = mapping.upsert(&conn, "x@y.z", &v).unwrap_err();
        assert!(err.to_string().contains("does not match wire key"));
    }

    /// End-to-end test from the runtime layer: a `maild.props.set`
    /// against a freshly bootstrapped accounts namespace must
    ///   (a) write the SQL accounts row,
    ///   (b) seed the six default mailboxes + calendar + addressbook,
    ///   (c) transition `_lifecycle` from Provisioning to Active.
    #[tokio::test]
    async fn end_to_end_set_creates_account_and_seeds_mds() {
        use cosmix_props::bus::mutation::PropsRouter;
        use cosmix_props::lifecycle::Lifecycle;
        use cosmix_props::record::Nseq;
        use cosmix_props::runtime::SetOpts;
        use cosmix_props::store::MergeMode;

        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("maild.db");
        let mds_dir = tmp.path().join("mds");
        std::fs::create_dir_all(&mds_dir).unwrap();

        // Maild's connection — runs the application schema.
        let app_conn = Connection::open(&db_path).unwrap();
        app_conn
            .execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;",
            )
            .unwrap();
        app_conn.execute_batch(crate::db::SCHEMA).unwrap();
        let db_conn = Arc::new(Mutex::new(app_conn));

        // Substrate's connection — separate handle to the same file.
        let props_conn = Connection::open(&db_path).unwrap();
        props_conn
            .execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;",
            )
            .unwrap();
        let store = Arc::new(SqliteStore::new("maild", props_conn).unwrap());

        let mds = Arc::new(cosmix_mds::SqliteCasMds::open(mds_dir.to_str().unwrap()).unwrap());
        let mailstore = Arc::new(SqliteMailStore::new(mds));

        let mut router = PropsRouter::new("maild");
        // Phase 2 wiring: account_overrides must register first so we
        // have its runtime in hand for AccountsHooks.
        let accounts_lock = Arc::new(std::sync::OnceLock::new());
        let overrides_rt =
            crate::props::account_overrides::register(&mut router, &store, accounts_lock.clone())
                .unwrap();
        let runtime = register(
            &mut router,
            &store,
            mailstore.clone(),
            db_conn.clone(),
            overrides_rt.clone(),
            Arc::new(OnceLock::new()),
        )
        .unwrap();
        accounts_lock
            .set(runtime.clone())
            .map_err(|_| ())
            .expect("accounts_lock empty");

        let mut m = std::collections::BTreeMap::new();
        m.insert("email".into(), PropValue::String("user@example.com".into()));
        m.insert(
            "password".into(),
            PropValue::String("$2b$12$abcdefghijklmnopqrstuv".into()),
        );
        m.insert("name".into(), PropValue::String("Test User".into()));
        m.insert("quota".into(), PropValue::Int(0));
        m.insert("spam_enabled".into(), PropValue::Bool(true));
        m.insert("spam_threshold".into(), PropValue::Float(0.5));
        let value = PropValue::Object(m);

        let opts = SetOpts {
            expected_version: None,
            merge: MergeMode::Replace,
            actor: Actor::operator("test"),
            cause: None,
            ts_ms: 0,
        };
        let key = RecordKey::collection(namespace_name(), "user@example.com");

        let outcome = runtime
            .set(key.clone(), value, opts)
            .await
            .expect("set succeeds");

        // (c) Saga: after_set Ok → Active. Complete event present.
        let complete = outcome
            .complete_event
            .expect("saga emits Complete event on after_set Ok");
        match &complete.lifecycle {
            Some(Lifecycle::Active) => {}
            other => panic!("expected Lifecycle::Active, got {other:?}"),
        }
        assert!(outcome.set_event.nseq > Nseq::zero());

        // (a) SQL row written + (b) calendar/addressbook seeded.
        // Scope the MutexGuard explicitly so clippy can prove it does
        // not live across the next `.await` below — `drop(conn)` alone
        // doesn't satisfy `clippy::await_holding_lock`.
        let id: i32 = {
            let conn = db_conn.lock().unwrap();
            let (id, email, name): (i32, String, Option<String>) = conn
                .query_row(
                    "SELECT id, email, name FROM accounts WHERE email = ?1",
                    params!["user@example.com"],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap();
            assert_eq!(email, "user@example.com");
            assert_eq!(name.as_deref(), Some("Test User"));
            let cal_count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM calendars WHERE account_id = ?1",
                    params![id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(cal_count, 1, "default calendar seeded");
            let ab_count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM addressbooks WHERE account_id = ?1",
                    params![id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(ab_count, 1, "default addressbook seeded");
            id
        };

        // (b) MDS mailboxes seeded — six roles present.
        let mboxes = mailstore.list_mailboxes(id).unwrap();
        assert_eq!(mboxes.len(), 6, "six default mailboxes seeded");
        for (_, role, _) in DEFAULT_MAILBOXES {
            assert!(
                mboxes.iter().any(|m| m.role == Some(role)),
                "role {role:?} must be present after seeding"
            );
        }

        // Saga replay idempotency: re-running set against the same key
        // must not duplicate mailboxes or seed rows.
        let mut m2 = std::collections::BTreeMap::new();
        m2.insert("email".into(), PropValue::String("user@example.com".into()));
        m2.insert(
            "password".into(),
            PropValue::String("$2b$12$differenthashedstring".into()),
        );
        m2.insert("name".into(), PropValue::String("Test User Renamed".into()));
        m2.insert("quota".into(), PropValue::Int(0));
        m2.insert("spam_enabled".into(), PropValue::Bool(true));
        m2.insert("spam_threshold".into(), PropValue::Float(0.5));
        let opts2 = SetOpts {
            expected_version: None,
            merge: MergeMode::Replace,
            actor: Actor::operator("test"),
            cause: None,
            ts_ms: 0,
        };
        runtime
            .set(key.clone(), PropValue::Object(m2), opts2)
            .await
            .expect("replay set succeeds");
        let mboxes2 = mailstore.list_mailboxes(id).unwrap();
        assert_eq!(mboxes2.len(), 6, "replay must not duplicate mailboxes");
    }

    /// C8 — Saga failure path. SPEC 12 §6.5 requires that an
    /// `after_set` `Err` transitions the row to `Lifecycle::Failed
    /// { reason }` with the substrate's sanitised reason string, while
    /// the SQL record itself stays durable for operator inspection.
    /// We exercise this with a test-local [`HookHandler`] whose
    /// `after_set` always returns `Err` — the same wire contract the
    /// production seeder satisfies when it hits a hand-edit conflict
    /// (the [`seed_default_mailboxes_idempotent`] root-name-without-role
    /// branch), but the failure shape is injected directly so the test
    /// does not depend on the seeder's specific failure modes.
    #[tokio::test]
    async fn saga_failure_path_after_set_err_marks_failed() {
        use cosmix_props::lifecycle::Lifecycle;
        use cosmix_props::runtime::SetOpts;
        use cosmix_props::store::MergeMode;

        struct FailingHook;
        impl HookHandler for FailingHook {
            fn after_set<'a>(&'a self, _ctx: &'a HookCtx) -> HookFuture<'a, ()> {
                Box::pin(async move {
                    Err(HookError::hook(
                        "seed_default_mailboxes: synthetic test failure",
                    ))
                })
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("maild.db");
        let app_conn = Connection::open(&db_path).unwrap();
        app_conn
            .execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;",
            )
            .unwrap();
        app_conn.execute_batch(crate::db::SCHEMA).unwrap();
        let db_conn = Arc::new(Mutex::new(app_conn));

        let props_conn = Connection::open(&db_path).unwrap();
        props_conn
            .execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;",
            )
            .unwrap();
        let store = Arc::new(SqliteStore::new("maild", props_conn).unwrap());

        // Register the namespace with the failing hook so commit_set's
        // mapping.upsert still writes the real SQL row and the Saga
        // transition lands on Failed when after_set returns Err.
        let spec_a = spec(Hooks::new(FailingHook));
        store
            .register_namespace(&spec_a, Arc::new(AccountsMapping))
            .unwrap();
        let spec_b = spec(Hooks::new(FailingHook));
        let store_trait: Arc<dyn cosmix_props::store::PropertyStore> = store.clone();
        let runtime = Runtime::new("maild", spec_b, store_trait);

        let mut m = std::collections::BTreeMap::new();
        m.insert("email".into(), PropValue::String("fail@example.com".into()));
        m.insert(
            "password".into(),
            PropValue::String("$2b$12$xxxxxxxxxxxxxxxxxxxxxx".into()),
        );
        m.insert("name".into(), PropValue::String("Fail".into()));
        m.insert("quota".into(), PropValue::Int(0));
        m.insert("spam_enabled".into(), PropValue::Bool(true));
        m.insert("spam_threshold".into(), PropValue::Float(0.5));

        let opts = SetOpts {
            expected_version: None,
            merge: MergeMode::Replace,
            actor: Actor::operator("test"),
            cause: None,
            ts_ms: 0,
        };
        let key = RecordKey::collection(namespace_name(), "fail@example.com");
        let outcome = runtime
            .set(key.clone(), PropValue::Object(m), opts)
            .await
            .expect("Saga returns Ok with a Failed lifecycle, not RuntimeError");

        // (1) Saga emits a Complete event carrying Lifecycle::Failed
        //     with the sanitised reason from after_set.
        let complete = outcome
            .complete_event
            .expect("Saga emits Complete event for the Failed transition");
        match &complete.lifecycle {
            Some(Lifecycle::Failed { reason }) => {
                assert!(
                    reason.contains("synthetic test failure"),
                    "Failed reason should surface the hook error: got {reason:?}"
                );
            }
            other => panic!("expected Lifecycle::Failed, got {other:?}"),
        }

        // (2) records.changed contract: both the Set event and the
        //     Complete event are durable in __props_events (this is
        //     the substrate side of the §5.5 records.changed body —
        //     the live fan-out publisher is deferred from C7).
        assert!(outcome.set_event.nseq > cosmix_props::record::Nseq::zero());
        assert!(complete.nseq > outcome.set_event.nseq);

        // (3) SQL row remains durable for operator inspection — the
        //     substrate's commit_set tx committed before after_set ran.
        let conn = db_conn.lock().unwrap();
        let (id, email): (i32, String) = conn
            .query_row(
                "SELECT id, email FROM accounts WHERE email = ?1",
                params!["fail@example.com"],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("account row durable after Saga failure");
        assert!(id > 0);
        assert_eq!(email, "fail@example.com");
    }

    #[test]
    fn validate_email_shape_reserves_opaque_vtoken_namespace() {
        // A 6/10-char Crockford-base32 local-part is the reserved opaque vtoken
        // shape — rejected as an account email (C9 namespace reservation).
        assert!(AccountsHooks::validate_email_shape("k4m9p2@example.org").is_err());
        assert!(AccountsHooks::validate_email_shape("k4m9p2x7qe@example.org").is_err());
        // Normal mailbox names (containing i/l/o/u, or the wrong length) pass.
        assert!(AccountsHooks::validate_email_shape("mark@example.org").is_ok());
        assert!(AccountsHooks::validate_email_shape("info22@example.org").is_ok());
    }

    #[tokio::test]
    async fn before_set_rejects_bad_email() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::db::SCHEMA).unwrap();
        let db_conn = Arc::new(Mutex::new(conn));
        let mds_dir = tempfile::tempdir().unwrap();
        let mds =
            Arc::new(cosmix_mds::SqliteCasMds::open(mds_dir.path().to_str().unwrap()).unwrap());
        let mailstore = Arc::new(SqliteMailStore::new(mds));

        // Minimal overrides runtime backed by an in-memory store so
        // AccountsHooks::new has something to hold. The hook short-
        // circuits on the email-shape check before reaching the
        // overrides_runtime, so the runtime is unused at hook time —
        // we just need it to exist.
        let overrides_rt = Arc::new(cosmix_props::runtime::Runtime::new(
            "maild",
            crate::props::account_overrides::spec(Hooks::noop()),
            Arc::new(cosmix_props::memory::MemoryStore::new("maild"))
                as Arc<dyn cosmix_props::store::PropertyStore>,
        ));
        let hooks = AccountsHooks::new(db_conn, mailstore, overrides_rt, Arc::new(OnceLock::new()));

        let ctx = HookCtx {
            key: RecordKey::collection(namespace_name(), "not-an-email"),
            old: None,
            new: Some(PropValue::Object(Default::default())),
            version: Version::zero(),
            actor: Actor::service("test"),
            merge: Some(cosmix_props::store::MergeMode::Replace),
            origin: cosmix_props::WriteOrigin::caller(),
        };
        let err = hooks.before_set(&ctx).await.unwrap_err();
        assert!(matches!(err, HookError::Validation { .. }));
    }
}
