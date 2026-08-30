#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use cosmix_maild::{config, db, runtime, smtp};

use anyhow::Result;
use clap::{Parser, Subcommand};

/// `--version` line with git sha + build time (version-discovery
/// contract). `COSMIX_*` set by build.rs → `cosmix_buildinfo::emit()`.
const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("COSMIX_GIT_SHA"),
    ", built ",
    env!("COSMIX_BUILD_TIME"),
    ")"
);

#[derive(Parser)]
#[command(name = "cosmix-maild", version = VERSION, about = "Minimal JMAP + SMTP server")]
struct Cli {
    /// Config file path
    #[arg(short, long)]
    config: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the JMAP + SMTP server
    Serve,
    /// Run database migrations
    Migrate,
    /// Account management
    Account {
        #[command(subcommand)]
        action: AccountAction,
    },
    /// Per-account rules-engine overrides
    AccountOverrides {
        #[command(subcommand)]
        action: AccountOverridesAction,
    },
    /// SMTP queue management
    Queue {
        #[command(subcommand)]
        action: QueueAction,
    },
    /// Per-domain SPEC 12 namespace (Phase 2)
    Domain {
        #[command(subcommand)]
        action: DomainAction,
    },
    /// Manage `maild.aliases` — Phase 1 local-target email aliases.
    /// Every mutation goes through `maild.props.{set,delete}` so the
    /// substrate's `before_set` validation (canonical form, self-alias,
    /// no-chaining) observes it identically to a programmatic peer.
    Alias {
        #[command(subcommand)]
        action: AliasAction,
    },
    /// Read-only inspection of the `maild.tls_identities` namespace.
    /// Rows are reconciled from `[[tls.identity]]` config at startup;
    /// every `maild.tls.reload` reprojects startup config plus the
    /// `maild.domains.tls_identity_name` projection. The namespace is
    /// not operator-writable over Bus, so this CLI exposes `list` +
    /// `show` only.
    TlsIdentity {
        #[command(subcommand)]
        action: TlsIdentityAction,
    },
    /// Read-only inspection of the singleton `maild.engine_config`
    /// namespace. Writes are intentionally not exposed by this CLI:
    /// the namespace is `require_version=true` and `MergeMode::Replace`
    /// requires the full 12-field body, which is not safe to
    /// construct from position-style CLI flags. (Partial
    /// `MergeMode::Patch` writes ARE supported by the substrate, but
    /// this CLI does not expose either Patch or Replace.) Mutations
    /// go via `maild.props.set` from an ops script that owns the
    /// body shape.
    EngineConfig {
        #[command(subcommand)]
        action: EngineConfigAction,
    },
    /// Per-domain DKIM key lifecycle. Wraps the `maild.dkim.{generate,
    /// rotate,retire}` Bus verbs as a transient anonymous client; the
    /// daemon's `[[dkim.domain]]` operator-managed baseline is refused
    /// (substrate verbs only touch substrate-managed domains).
    Dkim {
        #[command(subcommand)]
        action: DkimAction,
    },
    /// Inspection + refresh verbs against the live rules engine.
    /// Wraps `maild.rules.stats` and `maild.rules.reload`. The
    /// diagnostic `maild.rules.explain` verb takes a full envelope
    /// (`account_id`, `envelope_from`, `envelope_to[]`, `peer_ip`,
    /// base64 message) that is not ergonomic to assemble from CLI
    /// flags; ops scripts that need it call the Bus verb directly.
    Rules {
        #[command(subcommand)]
        action: RulesAction,
    },
    /// Read-only inspection of the Bayesian classifier. Wraps the
    /// `maild.bayesian.stats` Bus verb. The diagnostic
    /// `maild.bayesian.classify` verb takes a base64 message body
    /// and is not exposed by this CLI for the same reason as
    /// `rules.explain` — ops scripts that need it call Bus directly.
    /// Corpus replacement is likewise agent-operated through
    /// `maild.bayesian.rebuild` and observed through
    /// `maild.bayesian.rebuild_status`; neither mutating/job verb is
    /// duplicated as a local CLI subcommand.
    Bayesian {
        #[command(subcommand)]
        action: BayesianAction,
    },
    /// TLS resolver lifecycle verbs (`maild.tls.*`). Distinct from
    /// `tls-identity` (which is read-only inspection of the
    /// substrate `maild.tls_identities` namespace) — the Bus
    /// `maild.tls.reload` verb is a daemon-global refresh primitive
    /// that re-reads PEMs from disk, re-projects the
    /// `maild.tls_identities` namespace from the merged config +
    /// substrate identity set, and atomically swaps the live
    /// resolver. Idempotent and fail-loud-and-keep: on any failure
    /// the prior resolver continues serving.
    Tls {
        #[command(subcommand)]
        action: TlsAction,
    },
}

/// Per-domain DKIM key lifecycle verbs. The wire payload is a JSON
/// body of `{domain, selector, algorithm?}` — `algorithm` is only
/// honoured by `generate`. The substrate writes the PEM under
/// `<key_root>/<domain>/<selector>.pem` atomically (mode 0600), then
/// commits the `maild.domains` row in Replace mode, then rebuilds
/// the in-memory signer.
#[derive(Subcommand)]
enum DkimAction {
    /// Generate a fresh DKIM keypair for a (domain, selector) pair.
    /// The first selector becomes active; subsequent generates leave
    /// the existing active in place. Prints the DNS TXT record the
    /// operator must publish before signing traffic relies on it.
    Generate {
        /// Fully qualified domain name (substrate-managed only).
        #[arg(long)]
        domain: String,
        /// Selector label (e.g. `s2026a`).
        #[arg(long)]
        selector: String,
        /// Signing algorithm: `rsa-sha256` (default, 2048-bit) or
        /// `ed25519-sha256`.
        #[arg(long, default_value = "rsa-sha256")]
        algorithm: String,
    },
    /// Promote an already-generated selector to active. The PEM must
    /// already be on disk — `rotate` will not generate keys.
    Rotate {
        /// Fully qualified domain name.
        #[arg(long)]
        domain: String,
        /// Selector to promote.
        #[arg(long)]
        selector: String,
    },
    /// Remove a non-active selector. The active selector cannot be
    /// retired; rotate to another selector first.
    Retire {
        /// Fully qualified domain name.
        #[arg(long)]
        domain: String,
        /// Selector to retire.
        #[arg(long)]
        selector: String,
    },
}

/// Read-only inspection of the `maild.engine_config` singleton.
/// `show` calls `maild.props.get namespace=engine_config` and omits
/// the `key` header (per `Cardinality::Singleton` semantics —
/// `build_record_key` accepts either an absent or empty-string key
/// for singletons) and pretty-prints the substrate-wrapped record.
#[derive(Subcommand)]
enum EngineConfigAction {
    /// Show the current `maild.engine_config` row.
    Show,
}

/// Inspection + refresh verbs against the live rules engine.
#[derive(Subcommand)]
enum RulesAction {
    /// Pretty-print `maild.rules.stats`: pack metadata, verdict
    /// totals, and the per-rule hit map. `--top-n` bounds the
    /// per-rule slice (default 256, ceiling 4096 enforced
    /// daemon-side); pass `--top-n 0` for a cardinality-only read
    /// that skips the O(N) per-rule clone.
    Stats {
        /// Bound the `per_rule` slice in the response. Omitted →
        /// daemon default (256). Daemon caps at 4096; values above
        /// the cap are silently clamped daemon-side.
        #[arg(long)]
        top_n: Option<u64>,
    },
    /// Re-read the configured rules-pack file from disk and swap
    /// it into the live `RuleEngine` atomically. Returns
    /// `{pack_version, rules_loaded, rules_failed}` on success.
    /// Fail-loud-and-keep: on a file-load error the prior pack
    /// continues serving; `rules_failed` lists per-rule parse
    /// errors that the loader skipped. No-op if the daemon was
    /// started without `rules_pack_path` configured (returns the
    /// current state unchanged).
    Reload,
}

/// TLS resolver lifecycle verbs (`maild.tls.*`).
#[derive(Subcommand)]
enum TlsAction {
    /// Re-read substrate-declared TLS identities and the startup
    /// `[[tls.identity]]` snapshot, rebuild the SNI cert resolver,
    /// re-project the `maild.tls_identities` namespace, and swap
    /// the live resolver atomically. The next accepted connection
    /// sees the new resolver; in-flight handshakes drain on the
    /// captured prior `Arc<ServerConfig>`. On any failure (missing
    /// PEM, key↔cert mismatch, path-escape, parse error) the prior
    /// resolver continues serving — no half-valid swap.
    Reload,
}

/// Read-only inspection verbs against the Bayesian classifier.
#[derive(Subcommand)]
enum BayesianAction {
    /// Pretty-print `maild.bayesian.stats` for an account id. The
    /// id is the maild.accounts PK (an integer); the daemon's
    /// `parse_account_id` accepts either a JSON integer or an
    /// all-ASCII-digit string, so this CLI passes it as the latter.
    Stats {
        /// Account id (maild.accounts PK). Passed to the daemon as
        /// typed; the daemon's `parse_account_id` (bus/bayesian.rs)
        /// is the digit-only trust boundary that prevents
        /// corpus-tree path escape via the wire payload. This CLI
        /// does NOT pre-validate — that would duplicate the daemon
        /// check and would mangle the daemon's truthful error
        /// message ("account_id must be a non-negative integer,
        /// got …") for operator debugging.
        account_id: String,
    },
}

/// Read-only inspection verbs against `maild.tls_identities`. The
/// namespace's AuthPolicy doesn't grant a write cap to any peer and
/// `before_set` / `before_delete` reject operator writes regardless;
/// `add`/`remove` would be dead arms.
#[derive(Subcommand)]
enum TlsIdentityAction {
    /// List every `maild.tls_identities` row (server name + default
    /// + SNI flags + source).
    List,
    /// Show the full `maild.tls_identities` row for one server name.
    Show {
        /// SNI server name (lower-cased, RFC 6066) — the substrate
        /// row's primary key.
        server_name: String,
    },
}

/// Per-domain substrate verbs — Phase 2 plan §6.
///
/// Bus-only client of the running maild daemon's `maild.domains`
/// namespace. Every mutation goes through `maild.props.set` / `delete`
/// so the substrate's `before_set` validation and `records.changed`
/// fan-out observe it identically to a programmatic peer.
#[derive(Subcommand)]
enum DomainAction {
    /// Create a fresh `maild.domains` row with schema defaults.
    /// FQDN is validated locally before the Bus hop so a typo surfaces
    /// without a broker round-trip.
    Add {
        /// Fully qualified domain name (lowercase, RFC 1035 labels)
        fqdn: String,
    },
    /// List every `maild.domains` row (FQDN + role + enabled)
    List,
    /// Show the full `maild.domains` row for a single FQDN
    Show {
        /// Fully qualified domain name
        fqdn: String,
    },
    /// Patch one field on an existing `maild.domains` row
    /// (`merge: patch` — the substrate's `before_set` validates the
    /// merged result, so an invalid value or unknown field is
    /// rejected server-side with the real reason). VALUE is parsed
    /// as JSON first (`null` clears an option field, `true`/`false`
    /// for booleans, `'["a"]'` for lists) and falls back to a plain
    /// string (so `mail.example.com` needs no extra quoting).
    Set {
        /// Fully qualified domain name
        fqdn: String,
        /// Schema field name (e.g. helo_identity, primary_hostname)
        field: String,
        /// New value (JSON literal or bare string)
        value: String,
    },
    /// Remove a `maild.domains` row. Idempotent: a missing row is
    /// reported but not treated as an error.
    Remove {
        /// Fully qualified domain name
        fqdn: String,
    },
}

/// `cosmix-maild alias` — manage local email aliases. Mail to an alias
/// is delivered to its target account, and the authenticated target may
/// submit `From:` the alias.
#[derive(Subcommand)]
enum AliasAction {
    /// Add an alias `<alias> -> <target>` (both canonicalised locally
    /// before the Bus hop). The target must be a real local account and
    /// must not itself be an alias (Phase 1 single-hop).
    Add {
        /// The alias address (e.g. mc@example.com)
        alias: String,
        /// The target local account (e.g. admin@example.com)
        target: String,
    },
    /// List every alias (`address -> target`, enabled flag).
    List,
    /// Remove an alias. Idempotent: a missing row is reported, not an error.
    Remove {
        /// The alias address to remove
        alias: String,
    },
}

#[derive(Subcommand)]
enum AccountAction {
    /// Add a new account.
    ///
    /// Writes the FULL row (`merge=replace`), so it is a create — not an
    /// update. It therefore refuses an account that already exists unless
    /// `--force`; re-running `add` on a live account would silently reset
    /// `name`/`quota`/`spam_*`/`mfa_enabled` to the defaults hardcoded below
    /// while reporting "Created". To change only a password, use
    /// `account passwd`.
    Add {
        /// Email address
        email: String,
        /// Password. Omit to be prompted: a password in argv is visible in
        /// `ps`, in shell history, and in the log of any script that invokes
        /// us. Positional form retained for existing scripted callers.
        password: Option<String>,
        /// Display name
        #[arg(short, long)]
        name: Option<String>,
        /// Read the password from stdin (one line) instead of prompting.
        #[arg(long)]
        stdin: bool,
        /// Replace an existing account's row (see the refusal above).
        #[arg(long)]
        force: bool,
    },
    /// Change an account's password.
    ///
    /// Patches ONLY `maild.accounts.password` (bcrypt hashed client-side,
    /// exactly as `add` does), so name/quota/spam settings/mfa are preserved —
    /// the substrate merges the patch over the stored record before
    /// persistence. `AccountsHooks::after_set` clears the Basic-credential
    /// cache post-commit, so the OLD password stops working immediately rather
    /// than lingering for the cache TTL.
    Passwd {
        /// Account email
        email: String,
        /// Read the new password from stdin (one line) instead of prompting.
        #[arg(long)]
        stdin: bool,
    },
    /// Check whether a password authenticates, changing nothing.
    ///
    /// A bcrypt hash cannot be read back (`show` redacts the field), so the
    /// only meaningful "read" of a password is a verification. Runs daemon-side
    /// via `maild.accounts.verify`, reusing the same `auth::basic::verify` path
    /// IMAP/JMAP Basic auth uses — the stored hash never crosses the wire.
    /// Exit status: 0 = valid, 1 = invalid.
    Verify {
        /// Account email
        email: String,
        /// Read the password from stdin (one line) instead of prompting.
        #[arg(long)]
        stdin: bool,
    },
    /// Lock an account: authentication is refused, mailbox and password kept.
    ///
    /// Daemon-side (`maild.accounts.lock`), because the stored hash must be
    /// read to be preserved. Prefixes the bcrypt hash with `!`: `bcrypt::verify`
    /// fails closed on a malformed hash (`unwrap_or(false)`), so no password can
    /// match, while the original hash survives intact for `unlock`. Idempotent.
    Lock {
        /// Account email
        email: String,
    },
    /// Unlock a locked account, restoring the password that worked before.
    ///
    /// Strips the `!` prefix written by `lock` (`maild.accounts.unlock`).
    /// Idempotent; an account that is not locked is left alone.
    Unlock {
        /// Account email
        email: String,
    },
    /// Enable or disable email-2FA for an account. Sets
    /// `maild.accounts.mfa_enabled` via a `props.set` Patch, so the password
    /// and every other field are preserved (only the flag changes). webd reads
    /// this at login to decide whether to require an emailed second factor (P3).
    Mfa {
        /// Account email
        email: String,
        /// `on`/`off` (also accepts true/false, yes/no, 1/0).
        enabled: String,
    },
    /// List all accounts
    List,
    /// Show the full `maild.accounts` row for a single email.
    /// Reads via `maild.props.get` so every substrate-visible field
    /// (quota, spam_enabled, spam_threshold, etc.) is rendered —
    /// `list` only shows email + display name. The `password`
    /// field is `secret: true` and is redacted by the substrate
    /// for any peer lacking the `props.read:maild.accounts:secrets`
    /// capability.
    Show {
        /// Account email
        email: String,
    },
    /// Delete an account
    Delete {
        /// Email address
        email: String,
    },
    /// Backfill default JMAP mailboxes (Inbox/Drafts/Sent/Junk/Trash/Archive)
    /// in MDS for one or all accounts. Idempotent: roles already present are
    /// skipped. Use to migrate accounts created before the Phase 6/7 cutover,
    /// whose legacy SQL mailbox seed is no longer consulted.
    SeedMailboxes {
        /// Email of a single account to seed. Omit to seed every account.
        #[arg(short, long)]
        email: Option<String>,
    },
}

/// Per-account rules-engine overrides — Phase 2 C3.
///
/// All three subcommands are Bus-only clients of the running maild
/// daemon's `maild.account_overrides` namespace. The CLI never touches
/// the SQL or substrate files directly; every mutation goes through
/// `maild.props.{get,set,delete}` so the substrate's `before_set`
/// validation, audit stream, and `records.changed` fan-out all observe
/// it identically to a programmatic peer.
#[derive(Subcommand)]
enum AccountOverridesAction {
    /// Show the override row for an account (or report "no overrides")
    Get {
        /// Account email
        #[arg(short, long)]
        email: String,
    },
    /// Create or update the override row for an account.
    ///
    /// Default is full replacement: omitted flags revert to schema
    /// defaults (empty lists, null threshold). Use `--merge` for a
    /// per-field patch that preserves unspecified fields; patch sends
    /// `if_version` from a prior `get` for concurrency, with `--force`
    /// as the escape hatch for one-shot ops scripts.
    Set {
        /// Account email (must already exist as a maild account)
        #[arg(short, long)]
        email: String,
        /// Override the rules-engine spam threshold (finite, non-negative).
        /// Defaults to schema null on full replace if omitted.
        #[arg(long)]
        threshold: Option<f64>,
        /// Rule id to skip for this account; pass multiple times.
        #[arg(long = "disable-rule", value_name = "RULE_ID")]
        disable_rule: Vec<String>,
        /// Sender glob whose mail bypasses spam routing; pass multiple times.
        #[arg(long = "allow", value_name = "GLOB")]
        allow: Vec<String>,
        /// Sender glob whose mail routes straight to Junk; pass multiple times.
        #[arg(long = "block", value_name = "GLOB")]
        block: Vec<String>,
        /// Send `merge: patch` instead of full replace (preserves
        /// fields not mentioned on this command line).
        #[arg(long)]
        merge: bool,
        /// In patch mode, skip the prior-get `if_version` round-trip.
        /// Has no effect on full replace.
        #[arg(long)]
        force: bool,
    },
    /// Remove the override row for an account (engine reverts to defaults).
    Clear {
        /// Account email
        #[arg(short, long)]
        email: String,
    },
}

#[derive(Subcommand)]
enum QueueAction {
    /// List queued messages
    List,
    /// Flush queue (retry all now)
    Flush,
}

/// Does this account exist? A `maild.props.get` whose `not_found` is a clean
/// `false` rather than an error, so callers can branch on it.
async fn account_exists(client: &cosmix_client::NodedClient, email: &str) -> Result<bool> {
    use std::collections::BTreeMap;
    let mut headers = BTreeMap::new();
    headers.insert("namespace".to_string(), "accounts".to_string());
    headers.insert("key".to_string(), email.to_string());
    let (rc, body, _) = client
        .call_with_headers_raw("maild", "maild.props.get", &headers, "")
        .await?;
    if rc == 0 {
        return Ok(true);
    }
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
    if parsed.get("error_code").and_then(|v| v.as_str()) == Some("not_found") {
        return Ok(false);
    }
    let msg = parsed
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or(if body.is_empty() {
            "unknown error"
        } else {
            &body
        });
    Err(anyhow::anyhow!("account lookup failed (rc={rc}): {msg}"))
}

/// Preflight for the mutating single-field commands. Without it a Patch over a
/// non-existent row is treated as a CREATE and rejected for the missing
/// required `password` — a baffling error for an operator who merely typo'd the
/// address (the same trap `mfa` documents).
async fn require_account(client: &cosmix_client::NodedClient, email: &str) -> Result<()> {
    if account_exists(client, email).await? {
        Ok(())
    } else {
        Err(anyhow::anyhow!("No such account: {email}"))
    }
}

/// Obtain a NEW password: from stdin (scripting) or a no-echo TTY prompt with
/// confirmation. Never from argv — an argv password leaks into `ps`, shell
/// history, and the log of any script that shells out to us.
fn read_new_password(from_stdin: bool) -> Result<String> {
    if from_stdin {
        return read_password_stdin();
    }
    let pw = rpassword::prompt_password("New password: ")?;
    if pw.is_empty() {
        return Err(anyhow::anyhow!("password must not be empty"));
    }
    let again = rpassword::prompt_password("Retype new password: ")?;
    if pw != again {
        return Err(anyhow::anyhow!("passwords do not match"));
    }
    Ok(pw)
}

/// Obtain an EXISTING password to check (no confirmation — we are not setting
/// it, and a typo simply reports "invalid").
fn read_existing_password(from_stdin: bool) -> Result<String> {
    if from_stdin {
        return read_password_stdin();
    }
    let pw = rpassword::prompt_password("Password: ")?;
    if pw.is_empty() {
        return Err(anyhow::anyhow!("password must not be empty"));
    }
    Ok(pw)
}

/// One line from stdin, trailing newline stripped. A trailing `\r` is stripped
/// too so a CRLF heredoc doesn't silently set a password with a carriage return
/// in it — a failure that would present as "the password is simply wrong".
fn read_password_stdin() -> Result<String> {
    use std::io::BufRead;
    let mut buf = String::new();
    std::io::stdin().lock().read_line(&mut buf)?;
    let pw = buf.trim_end_matches(['\n', '\r']).to_string();
    if pw.is_empty() {
        return Err(anyhow::anyhow!("no password on stdin"));
    }
    Ok(pw)
}

/// Run a CLI `account` subcommand as a transient Bus client of the
/// already-running maild daemon. The CLI no longer touches the SQL
/// database or MDS directly — every mutation goes through the
/// `maild.props.*` / `maild.accounts.*` Bus surfaces so the substrate's
/// Saga lifecycle, audit log, and live `records.changed` fan-out all
/// observe it. Requires the daemon to be running and reachable via
/// the WireGuard `/24`; a missing or unreachable broker surfaces as
/// a clear "daemon not reachable" error rather than a stale local
/// write.
async fn run_account_cli(action: AccountAction) -> Result<()> {
    use std::collections::BTreeMap;

    let client = cosmix_config::client_helpers::connect_anonymous_default()
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "maild daemon not reachable via broker ({e}). \
                 The CLI now requires the daemon to be running — \
                 `systemctl start cosmix-maild` (or equivalent) first."
            )
        })?;

    match action {
        AccountAction::Add {
            email,
            password,
            name,
            stdin,
            force,
        } => {
            // `add` sends the full row with `merge=replace` and the defaults
            // hardcoded below, so running it on an EXISTING account silently
            // resets that account's name/quota/spam_*/mfa_enabled — while
            // printing "Created". Refuse instead: a password change is
            // `account passwd`, and a deliberate row rewrite is `--force`.
            if !force && account_exists(&client, &email).await? {
                return Err(anyhow::anyhow!(
                    "Account {email} already exists. `add` REPLACES the whole row \
                     (name/quota/spam settings/mfa reset to defaults). \
                     To change the password use `cosmix-maild account passwd {email}`; \
                     to rewrite the row anyway pass --force."
                ));
            }
            let password = match password {
                Some(p) => p,
                None => read_new_password(stdin)?,
            };
            // Bcrypt happens client-side. The substrate's
            // `maild.accounts` namespace declares `password` as a
            // `secret: true` string field with no transform inside
            // `upsert` (round-trip contract — see
            // `props/accounts.rs`), so the wire body carries the
            // hash verbatim.
            let hash = bcrypt::hash(&password, bcrypt::DEFAULT_COST)
                .map_err(|e| anyhow::anyhow!("bcrypt error: {e}"))?;
            let body = serde_json::json!({
                "email": email,
                "password": hash,
                "name": name,
                "quota": 0,
                "spam_enabled": true,
                "spam_threshold": 0.5,
            })
            .to_string();
            let mut headers = BTreeMap::new();
            headers.insert("namespace".to_string(), "accounts".to_string());
            headers.insert("key".to_string(), email.clone());
            headers.insert("merge".to_string(), "replace".to_string());
            // Saga lifecycle: `props.set` commits the SQL row inside
            // the substrate tx, then runs `after_set` to seed MDS
            // mailboxes and the per-account calendar/addressbook. A
            // failing `after_set` is NOT surfaced as `rc >= 10` — the
            // substrate keeps the row durable and projects
            // `{"lifecycle":"failed","reason":"…"}` with rc=0
            // (`cosmix-lib-props-store/src/runtime.rs` Saga branch). The CLI
            // must inspect the body to distinguish a healthy create
            // from a half-provisioned one; reporting success on a
            // failed `after_set` would leave operators with an
            // account row that cannot receive mail.
            let resp = client
                .call_with_headers("maild", "maild.props.set", &headers, &body)
                .await?;
            let lifecycle = resp
                .get("lifecycle")
                .and_then(|v| v.as_str())
                .unwrap_or("active");
            if lifecycle == "failed" {
                let reason = resp
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                return Err(anyhow::anyhow!(
                    "account {email} created but provisioning failed (lifecycle=failed): {reason}. \
                     The SQL row is durable; rerun `cosmix-maild account seed-mailboxes \
                     --email {email}` after addressing the underlying cause."
                ));
            }
            if force {
                println!("Replaced account {email} (row rewritten to defaults)");
            } else {
                println!("Created account {email}");
            }
        }
        AccountAction::Passwd { email, stdin } => {
            require_account(&client, &email).await?;
            let password = read_new_password(stdin)?;
            let hash = bcrypt::hash(&password, bcrypt::DEFAULT_COST)
                .map_err(|e| anyhow::anyhow!("bcrypt error: {e}"))?;
            // Patch ONLY `password`. The substrate merges the patch over the
            // stored record before persistence, so every other field survives
            // (same contract the `mfa` patch relies on). No `if_version` guard:
            // the field is a scalar, last-write-wins. `AccountsHooks::after_set`
            // clears the Basic-credential cache POST-commit, so the old password
            // is dead immediately rather than lingering for the cache TTL.
            let body = serde_json::json!({ "email": email, "password": hash }).to_string();
            let mut headers = BTreeMap::new();
            headers.insert("namespace".to_string(), "accounts".to_string());
            headers.insert("key".to_string(), email.clone());
            headers.insert("merge".to_string(), "patch".to_string());
            client
                .call_with_headers("maild", "maild.props.set", &headers, &body)
                .await?;
            println!("Password updated for {email}");
        }
        AccountAction::Verify { email, stdin } => {
            let password = read_existing_password(stdin)?;
            let mut headers = BTreeMap::new();
            headers.insert("email".to_string(), email.clone());
            let body = serde_json::json!({ "email": email, "password": password }).to_string();
            let resp = client
                .call_with_headers("maild", "maild.accounts.verify", &headers, &body)
                .await?;
            let valid = resp.get("valid").and_then(|v| v.as_bool()).unwrap_or(false);
            if valid {
                println!("VALID: password authenticates for {email}");
            } else {
                // Exit non-zero so `if cosmix-maild account verify …` works in a
                // Mix/shell guard without parsing stdout.
                println!("INVALID: password does not authenticate for {email}");
                std::process::exit(1);
            }
        }
        AccountAction::Lock { email } => {
            let mut headers = BTreeMap::new();
            headers.insert("email".to_string(), email.clone());
            let resp = client
                .call_with_headers("maild", "maild.accounts.lock", &headers, "")
                .await?;
            let changed = resp
                .get("changed")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if changed {
                println!("Locked {email} (auth refused; password hash preserved for unlock)");
            } else {
                println!("{email} was already locked");
            }
        }
        AccountAction::Unlock { email } => {
            let mut headers = BTreeMap::new();
            headers.insert("email".to_string(), email.clone());
            let resp = client
                .call_with_headers("maild", "maild.accounts.unlock", &headers, "")
                .await?;
            let changed = resp
                .get("changed")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if changed {
                println!("Unlocked {email} (prior password works again)");
            } else {
                println!("{email} was not locked");
            }
        }
        AccountAction::Mfa { email, enabled } => {
            let on = match enabled.to_ascii_lowercase().as_str() {
                "on" | "true" | "yes" | "1" => true,
                "off" | "false" | "no" | "0" => false,
                other => {
                    return Err(anyhow::anyhow!(
                        "mfa: expected on/off (or true/false), got {other:?}"
                    ));
                }
            };
            let mut headers = BTreeMap::new();
            headers.insert("namespace".to_string(), "accounts".to_string());
            headers.insert("key".to_string(), email.clone());
            // Preflight existence check: a Patch over a NON-existent row is
            // treated as a create and rejected for the missing required
            // `password` — a confusing error for an operator who simply typo'd
            // the email. Get first so an unknown account gets a clean message.
            let (get_rc, get_body, _) = client
                .call_with_headers_raw("maild", "maild.props.get", &headers, "")
                .await?;
            if get_rc != 0 {
                let parsed: serde_json::Value =
                    serde_json::from_str(&get_body).unwrap_or(serde_json::Value::Null);
                if parsed.get("error_code").and_then(|v| v.as_str()) == Some("not_found") {
                    return Err(anyhow::anyhow!("No such account: {email}"));
                }
                let msg = parsed.get("message").and_then(|v| v.as_str()).unwrap_or(
                    if get_body.is_empty() {
                        "unknown error"
                    } else {
                        &get_body
                    },
                );
                return Err(anyhow::anyhow!(
                    "mfa: account lookup failed (rc={get_rc}): {msg}"
                ));
            }
            // Patch ONLY `mfa_enabled`. The substrate merges the patch over the
            // stored record (password + every other field) BEFORE persistence,
            // so `value_to_account_fields` sees the merged row and the password
            // is preserved — no password round-trip needed. A scalar bool is
            // last-write-wins, so no if_version guard (unlike the list-valued
            // account-overrides patch).
            let body = serde_json::json!({ "email": email, "mfa_enabled": on }).to_string();
            headers.insert("merge".to_string(), "patch".to_string());
            client
                .call_with_headers("maild", "maild.props.set", &headers, &body)
                .await?;
            println!("Set mfa_enabled={on} for {email}");
        }
        AccountAction::List => {
            let mut headers = BTreeMap::new();
            headers.insert("namespace".to_string(), "accounts".to_string());
            let resp = client
                .call_with_headers("maild", "maild.props.list", &headers, "")
                .await?;
            let records = resp
                .get("records")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            if records.is_empty() {
                println!("No accounts.");
            } else {
                println!("{:<40} Name", "Email");
                println!("{}", "-".repeat(60));
                for r in records {
                    let email = r.get("key").and_then(|v| v.as_str()).unwrap_or("");
                    let name = r
                        .get("fields")
                        .and_then(|f| f.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    println!("{email:<40} {name}");
                }
            }
        }
        AccountAction::Show { email } => {
            let mut headers = BTreeMap::new();
            headers.insert("namespace".to_string(), "accounts".to_string());
            headers.insert("key".to_string(), email.clone());
            // Same `error_code=not_found` envelope as `delete`; surface
            // it as a clear "no such account" rather than a generic
            // rc-10 error.
            let (rc, body_str, _err_hdr) = client
                .call_with_headers_raw("maild", "maild.props.get", &headers, "")
                .await?;
            if rc == 0 {
                match serde_json::from_str::<serde_json::Value>(&body_str) {
                    Ok(resp) => {
                        let pretty = serde_json::to_string_pretty(&resp)
                            .unwrap_or_else(|_| body_str.clone());
                        println!("{pretty}");
                    }
                    Err(_) => println!("{body_str}"),
                }
            } else {
                let parsed: serde_json::Value =
                    serde_json::from_str(&body_str).unwrap_or(serde_json::Value::Null);
                let code = parsed.get("error_code").and_then(|v| v.as_str());
                if code == Some("not_found") {
                    println!("Account {email} not found");
                } else {
                    let msg = parsed
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or_else(|| {
                            if body_str.is_empty() {
                                "unknown error"
                            } else {
                                &body_str
                            }
                        });
                    let detail = match code {
                        Some(c) => format!("{c}: {msg}"),
                        None => msg.to_string(),
                    };
                    return Err(anyhow::anyhow!("show failed (rc={rc}): {detail}"));
                }
            }
        }
        AccountAction::Delete { email } => {
            let mut headers = BTreeMap::new();
            headers.insert("namespace".to_string(), "accounts".to_string());
            headers.insert("key".to_string(), email.clone());
            // The substrate returns `rc=10` with body
            // `{"error_code":"not_found", …}` when no record exists.
            // Use the raw call so we can branch on `error_code`
            // structurally rather than substring-matching a
            // human-readable string (the substrate body has the
            // canonical token; the `error` header is not set by the
            // `<svc>.props.*` responder).
            let (rc, body_str, _err_hdr) = client
                .call_with_headers_raw("maild", "maild.props.delete", &headers, "")
                .await?;
            if rc == 0 {
                println!("Deleted account {email}");
            } else {
                let parsed: serde_json::Value =
                    serde_json::from_str(&body_str).unwrap_or(serde_json::Value::Null);
                let code = parsed.get("error_code").and_then(|v| v.as_str());
                if code == Some("not_found") {
                    println!("Account {email} not found");
                } else {
                    let msg = parsed
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or_else(|| {
                            if body_str.is_empty() {
                                "unknown error"
                            } else {
                                &body_str
                            }
                        });
                    return Err(anyhow::anyhow!("delete failed (rc={rc}): {msg}"));
                }
            }
        }
        AccountAction::SeedMailboxes { email } => {
            let mut headers = BTreeMap::new();
            if let Some(addr) = &email {
                headers.insert("email".to_string(), addr.clone());
            }
            // The server returns `rc=10` when ANY target failed, but
            // the body still carries every successful entry. Use the
            // raw call so partial successes are rendered to the
            // operator instead of being swallowed by an early error.
            let (rc, body_str, _err_hdr) = client
                .call_with_headers_raw("maild", "maild.accounts.seed_mailboxes", &headers, "")
                .await?;
            let resp: serde_json::Value = if body_str.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::from_str(&body_str).unwrap_or(serde_json::Value::Null)
            };
            // Non-results error body (e.g. account-lookup failure
            // before any seeding ran) — no `results` array, just an
            // error string. Surface it directly.
            if rc != 0 && resp.get("results").is_none() {
                let msg = resp
                    .get("error")
                    .or_else(|| resp.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_else(|| {
                        if body_str.is_empty() {
                            "unknown error"
                        } else {
                            &body_str
                        }
                    });
                return Err(anyhow::anyhow!("seed_mailboxes failed (rc={rc}): {msg}"));
            }
            let results = resp
                .get("results")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            if results.is_empty() {
                println!("No accounts to seed.");
                return Ok(());
            }
            let mut failed = 0usize;
            for r in &results {
                let addr = r.get("email").and_then(|v| v.as_str()).unwrap_or("?");
                let id = r.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
                let err_field = r.get("error");
                if let Some(msg) = err_field.and_then(|v| v.as_str()) {
                    eprintln!("{addr} (id {id}): FAILED: {msg}");
                    failed += 1;
                    continue;
                }
                let created: Vec<String> = r
                    .get("created")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|s| s.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                if created.is_empty() {
                    println!("{addr} (id {id}): already seeded");
                } else {
                    println!("{addr} (id {id}): created {}", created.join(", "));
                }
            }
            if failed > 0 {
                return Err(anyhow::anyhow!(
                    "{failed} of the targeted account(s) failed to seed; see lines above"
                ));
            }
        }
    }
    Ok(())
}

/// Run a CLI `account-overrides` subcommand as a transient Bus client.
/// Mirrors [`run_account_cli`]: connects via the broker, calls the
/// `maild.props.*` surface for the `account_overrides` namespace, and
/// renders structured-error bodies into operator-readable messages.
async fn run_account_overrides_cli(action: AccountOverridesAction) -> Result<()> {
    use serde_json::Value as Json;
    use std::collections::BTreeMap;
    const NAMESPACE: &str = "account_overrides";

    let client = cosmix_config::client_helpers::connect_anonymous_default()
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "maild daemon not reachable via broker ({e}). \
                 `cosmix-maild account-overrides` requires the daemon to be \
                 running — `systemctl start cosmix-maild` (or equivalent) first."
            )
        })?;

    // Render an override row from a `maild.props.get` response body.
    fn render_overrides(email: &str, fields: &Json, version: Option<i64>) {
        let threshold = fields
            .get("threshold_override")
            .filter(|v| !v.is_null())
            .map(|v| v.to_string())
            .unwrap_or_else(|| "(default)".to_string());
        let disabled = fields
            .get("disabled_rules")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let allow = fields
            .get("allowlist_senders")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let block = fields
            .get("blocklist_senders")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        println!("Override for {email}:");
        if let Some(v) = version {
            println!("  version           {v}");
        }
        println!("  threshold         {threshold}");
        println!("  disabled_rules    [{disabled}]");
        println!("  allowlist_senders [{allow}]");
        println!("  blocklist_senders [{block}]");
    }

    // Pull `error_code` + `message` from a substrate response body.
    fn parse_error(body_str: &str) -> (Option<String>, String) {
        let parsed: Json = serde_json::from_str(body_str).unwrap_or(Json::Null);
        let code = parsed
            .get("error_code")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let msg = parsed
            .get("message")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| {
                if body_str.is_empty() {
                    "unknown error".to_string()
                } else {
                    body_str.to_string()
                }
            });
        (code, msg)
    }

    match action {
        AccountOverridesAction::Get { email } => {
            let mut headers = BTreeMap::new();
            headers.insert("namespace".to_string(), NAMESPACE.to_string());
            headers.insert("key".to_string(), email.clone());
            let (rc, body_str, _err_hdr) = client
                .call_with_headers_raw("maild", "maild.props.get", &headers, "")
                .await?;
            if rc == 0 {
                let resp: Json = serde_json::from_str(&body_str).unwrap_or(Json::Null);
                let version = resp.get("version").and_then(|v| v.as_i64());
                let fields = resp
                    .get("fields")
                    .cloned()
                    .unwrap_or(Json::Object(Default::default()));
                render_overrides(&email, &fields, version);
            } else {
                let (code, msg) = parse_error(&body_str);
                if code.as_deref() == Some("not_found") {
                    println!("No overrides for {email}");
                } else {
                    return Err(anyhow::anyhow!("get failed (rc={rc}): {msg}"));
                }
            }
        }
        AccountOverridesAction::Set {
            email,
            threshold,
            disable_rule,
            allow,
            block,
            merge,
            force,
        } => {
            // Build the JSON body from supplied flags only. On Replace
            // (the CLI default — see SPEC 12 §5.3) omitted fields take
            // schema defaults (empty lists, null threshold). On Patch
            // (`--merge`) omitted fields preserve the prior value.
            let mut body = serde_json::Map::new();
            body.insert("email".to_string(), Json::String(email.clone()));
            if let Some(t) = threshold {
                body.insert(
                    "threshold_override".to_string(),
                    serde_json::Number::from_f64(t)
                        .map(Json::Number)
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "threshold {t} is not a finite f64 (NaN/inf rejected before wire)"
                            )
                        })?,
                );
            }
            if !disable_rule.is_empty() {
                body.insert(
                    "disabled_rules".to_string(),
                    Json::Array(disable_rule.into_iter().map(Json::String).collect()),
                );
            }
            if !allow.is_empty() {
                body.insert(
                    "allowlist_senders".to_string(),
                    Json::Array(allow.into_iter().map(Json::String).collect()),
                );
            }
            if !block.is_empty() {
                body.insert(
                    "blocklist_senders".to_string(),
                    Json::Array(block.into_iter().map(Json::String).collect()),
                );
            }
            let body_str = Json::Object(body).to_string();

            let mut headers = BTreeMap::new();
            headers.insert("namespace".to_string(), NAMESPACE.to_string());
            headers.insert("key".to_string(), email.clone());
            // Plan: CLI default is full replacement (`merge: false`)
            // even though the SPEC 12 wire default is patch. Sending
            // the header explicitly removes the ambiguity for ops
            // scripts reading the Bus audit log.
            headers.insert(
                "merge".to_string(),
                if merge { "patch" } else { "replace" }.to_string(),
            );
            // Patch concurrency: round-trip an `if_version` from a
            // prior get so two operators editing overlapping list
            // fields can't race-and-overwrite. `--force` opts out;
            // Replace has nothing to merge so the version is ignored.
            if merge && !force {
                let mut get_headers = BTreeMap::new();
                get_headers.insert("namespace".to_string(), NAMESPACE.to_string());
                get_headers.insert("key".to_string(), email.clone());
                let (get_rc, get_body, _) = client
                    .call_with_headers_raw("maild", "maild.props.get", &get_headers, "")
                    .await?;
                if get_rc == 0 {
                    // `version` MUST be present + parseable on a `rc=0`
                    // get response (SPEC 12 §8.3 wire envelope). If it
                    // isn't, the substrate is on a wire we don't
                    // understand — fail loudly rather than silently
                    // dropping the if_version round-trip, which would
                    // turn `--merge` into an unguarded patch.
                    let resp: Json = serde_json::from_str(&get_body).map_err(|e| {
                        anyhow::anyhow!(
                            "pre-patch get returned rc=0 but body did not parse as JSON: {e}; \
                             refusing to patch without if_version (pass --force to override)"
                        )
                    })?;
                    let version =
                        resp.get("version")
                            .and_then(|v| v.as_u64())
                            .ok_or_else(|| {
                                anyhow::anyhow!(
                                    "pre-patch get returned rc=0 but no `version` field; \
                             refusing to patch without if_version (pass --force to override)"
                                )
                            })?;
                    headers.insert("if_version".to_string(), version.to_string());
                } else {
                    let (code, msg) = parse_error(&get_body);
                    // `not_found` is fine — patch on a missing record
                    // is a fresh create; if_version is omitted so the
                    // substrate accepts the create without conflict.
                    if code.as_deref() != Some("not_found") {
                        return Err(anyhow::anyhow!(
                            "pre-patch get failed (rc={get_rc}): {msg}; \
                             use --force to skip the version round-trip"
                        ));
                    }
                }
            }

            let (rc, resp_body, _err_hdr) = client
                .call_with_headers_raw("maild", "maild.props.set", &headers, &body_str)
                .await?;
            if rc == 0 {
                let resp: Json = serde_json::from_str(&resp_body).unwrap_or(Json::Null);
                let created = resp
                    .get("created")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let verb = if created { "Created" } else { "Updated" };
                println!("{verb} overrides for {email}");
            } else {
                let (code, msg) = parse_error(&resp_body);
                if code.as_deref() == Some("validation_error")
                    && msg.starts_with(
                        cosmix_maild::props::account_overrides::ACCOUNT_NOT_FOUND_PREFIX,
                    )
                {
                    return Err(anyhow::anyhow!(
                        "account {email} does not exist — create it with \
                         `cosmix-maild account add <email> <password>` first"
                    ));
                }
                if code.as_deref() == Some("version_mismatch") {
                    return Err(anyhow::anyhow!(
                        "{msg}; re-run after `cosmix-maild account-overrides get \
                         --email {email}`, or pass --force to overwrite"
                    ));
                }
                return Err(anyhow::anyhow!("set failed (rc={rc}): {msg}"));
            }
        }
        AccountOverridesAction::Clear { email } => {
            let mut headers = BTreeMap::new();
            headers.insert("namespace".to_string(), NAMESPACE.to_string());
            headers.insert("key".to_string(), email.clone());
            let (rc, body_str, _err_hdr) = client
                .call_with_headers_raw("maild", "maild.props.delete", &headers, "")
                .await?;
            if rc == 0 {
                println!("Cleared overrides for {email}");
            } else {
                let (code, msg) = parse_error(&body_str);
                if code.as_deref() == Some("not_found") {
                    println!("No overrides for {email}");
                } else {
                    return Err(anyhow::anyhow!("clear failed (rc={rc}): {msg}"));
                }
            }
        }
    }
    Ok(())
}

/// Run a CLI `domain` subcommand as a transient Bus client.
/// Mirrors [`run_account_overrides_cli`]: connects via the broker,
/// calls the `maild.props.*` surface for the `domains` namespace, and
/// renders structured-error bodies into operator-readable messages.
/// `cosmix-maild alias {add,list,remove}` — drives the `maild.aliases`
/// namespace over `maild.props.{set,list,delete}` so the substrate's
/// `before_set` (canonical form, self-alias, no-chaining) is the single
/// validator regardless of caller.
async fn run_alias_cli(action: AliasAction) -> Result<()> {
    use std::collections::BTreeMap;
    const NAMESPACE: &str = "aliases";
    use cosmix_maild::props::aliases::canon;

    let client = cosmix_config::client_helpers::connect_anonymous_default()
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "maild daemon not reachable via broker ({e}). \
                 `cosmix-maild alias` requires the daemon to be running."
            )
        })?;

    match action {
        AliasAction::Add { alias, target } => {
            let alias = canon(&alias);
            let target = canon(&target);
            let body = serde_json::json!({
                "address": alias,
                "target": target,
                "enabled": true,
            })
            .to_string();
            let mut headers = BTreeMap::new();
            headers.insert("namespace".to_string(), NAMESPACE.to_string());
            headers.insert("key".to_string(), alias.clone());
            headers.insert("merge".to_string(), "replace".to_string());
            // Fresh-create: reject if the alias already exists.
            headers.insert("if_version".to_string(), "0".to_string());
            let (rc, resp_body, _err) = client
                .call_with_headers_raw("maild", "maild.props.set", &headers, &body)
                .await?;
            if rc == 0 {
                println!("Added alias {alias} -> {target}");
            } else {
                let parsed: serde_json::Value =
                    serde_json::from_str(&resp_body).unwrap_or(serde_json::Value::Null);
                let code = parsed
                    .get("error_code")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let msg = parsed
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or_else(|| {
                        if resp_body.is_empty() {
                            "unknown error"
                        } else {
                            &resp_body
                        }
                    });
                if code == "version_mismatch" {
                    return Err(anyhow::anyhow!(
                        "alias {alias} already exists; remove it first to repoint"
                    ));
                }
                return Err(anyhow::anyhow!("alias add failed (rc={rc}): {msg}"));
            }
        }
        AliasAction::List => {
            let mut headers = BTreeMap::new();
            headers.insert("namespace".to_string(), NAMESPACE.to_string());
            let (rc, body_str, _err) = client
                .call_with_headers_raw("maild", "maild.props.list", &headers, "")
                .await?;
            if rc != 0 {
                let parsed: serde_json::Value =
                    serde_json::from_str(&body_str).unwrap_or(serde_json::Value::Null);
                let msg = parsed
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or_else(|| {
                        if body_str.is_empty() {
                            "unknown error"
                        } else {
                            &body_str
                        }
                    });
                return Err(anyhow::anyhow!("alias list failed (rc={rc}): {msg}"));
            }
            let resp: serde_json::Value =
                serde_json::from_str(&body_str).unwrap_or(serde_json::Value::Null);
            let records = resp
                .get("records")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            if records.is_empty() {
                println!("No aliases.");
            } else {
                println!("{:<32} {:<32} Enabled", "Alias", "Target");
                println!("{}", "-".repeat(72));
                for r in records {
                    let addr = r.get("key").and_then(|v| v.as_str()).unwrap_or("");
                    let fields = r.get("fields");
                    let target = fields
                        .and_then(|f| f.get("target"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let enabled = fields
                        .and_then(|f| f.get("enabled"))
                        .and_then(|v| v.as_bool())
                        .unwrap_or(true);
                    println!("{addr:<32} {target:<32} {enabled}");
                }
            }
        }
        AliasAction::Remove { alias } => {
            let alias = canon(&alias);
            // Pre-get to prove absence (idempotent success) or capture
            // the version for a conditional delete (mirrors `domain remove`).
            let mut get_headers = BTreeMap::new();
            get_headers.insert("namespace".to_string(), NAMESPACE.to_string());
            get_headers.insert("key".to_string(), alias.clone());
            let (get_rc, get_body, _e) = client
                .call_with_headers_raw("maild", "maild.props.get", &get_headers, "")
                .await?;
            if get_rc != 0 {
                let parsed: serde_json::Value =
                    serde_json::from_str(&get_body).unwrap_or(serde_json::Value::Null);
                if parsed.get("error_code").and_then(|v| v.as_str()) == Some("not_found") {
                    println!("Alias {alias} not found");
                    return Ok(());
                }
                let msg = parsed
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or_else(|| {
                        if get_body.is_empty() {
                            "unknown error"
                        } else {
                            &get_body
                        }
                    });
                return Err(anyhow::anyhow!(
                    "alias remove pre-get failed (rc={get_rc}): {msg}"
                ));
            }
            let get_resp: serde_json::Value =
                serde_json::from_str(&get_body).unwrap_or(serde_json::Value::Null);
            let version = get_resp
                .get("version")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let mut headers = BTreeMap::new();
            headers.insert("namespace".to_string(), NAMESPACE.to_string());
            headers.insert("key".to_string(), alias.clone());
            headers.insert("if_version".to_string(), version.to_string());
            let (rc, body_str, _e) = client
                .call_with_headers_raw("maild", "maild.props.delete", &headers, "")
                .await?;
            if rc == 0 {
                println!("Removed alias {alias}");
            } else {
                let parsed: serde_json::Value =
                    serde_json::from_str(&body_str).unwrap_or(serde_json::Value::Null);
                let msg = parsed
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or_else(|| {
                        if body_str.is_empty() {
                            "unknown error"
                        } else {
                            &body_str
                        }
                    });
                return Err(anyhow::anyhow!("alias remove failed (rc={rc}): {msg}"));
            }
        }
    }
    Ok(())
}

async fn run_domain_cli(action: DomainAction) -> Result<()> {
    use std::collections::BTreeMap;
    const NAMESPACE: &str = "domains";

    // Pre-validate any input that doesn't need the broker BEFORE
    // connecting, so a typo surfaces as a clear validation error even
    // when the daemon is stopped (otherwise the caller would see a
    // misleading "daemon not reachable" diagnostic for input that
    // would have been rejected regardless).
    match &action {
        DomainAction::Add { fqdn }
        | DomainAction::Show { fqdn }
        | DomainAction::Set { fqdn, .. }
        | DomainAction::Remove { fqdn } => {
            cosmix_maild::props::domains::validate_fqdn(fqdn)
                .map_err(|e| anyhow::anyhow!("invalid fqdn {fqdn:?}: {e}"))?;
        }
        DomainAction::List => {}
    }

    let client = cosmix_config::client_helpers::connect_anonymous_default()
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "maild daemon not reachable via broker ({e}). \
                 `cosmix-maild domain` requires the daemon to be \
                 running — `systemctl start cosmix-maild` (or equivalent) first."
            )
        })?;

    match action {
        DomainAction::Add { fqdn } => {
            // `defaults_value` is the single source of truth for the
            // default row shape — every schema field present, list
            // fields empty, options null. PropValue is Serialize via
            // `#[serde(untagged)]`, so json wire body is the natural
            // form.
            let body_value = cosmix_maild::props::domains::defaults_value(&fqdn);
            let body_str = serde_json::to_string(&body_value)
                .map_err(|e| anyhow::anyhow!("failed to serialize defaults_value: {e}"))?;

            let mut headers = BTreeMap::new();
            headers.insert("namespace".to_string(), NAMESPACE.to_string());
            headers.insert("key".to_string(), fqdn.clone());
            headers.insert("merge".to_string(), "replace".to_string());
            // Fresh-create concurrency: `if_version: 0` rejects the
            // call if the row already exists (substrate returns
            // `version_mismatch`). Operators editing an existing row
            // must use a later phase's `domain update` verb.
            headers.insert("if_version".to_string(), "0".to_string());

            let (rc, resp_body, _err_hdr) = client
                .call_with_headers_raw("maild", "maild.props.set", &headers, &body_str)
                .await?;
            if rc == 0 {
                println!("Added domain {fqdn}");
            } else {
                let parsed: serde_json::Value =
                    serde_json::from_str(&resp_body).unwrap_or(serde_json::Value::Null);
                let code = parsed
                    .get("error_code")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let msg = parsed
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or_else(|| {
                        if resp_body.is_empty() {
                            "unknown error"
                        } else {
                            &resp_body
                        }
                    });
                if code == "version_mismatch" {
                    return Err(anyhow::anyhow!(
                        "domain {fqdn} already exists ({msg}); \
                         later-phase `domain update` verb will edit existing rows"
                    ));
                }
                return Err(anyhow::anyhow!("add failed (rc={rc}): {msg}"));
            }
        }
        DomainAction::Set { fqdn, field, value } => {
            // The namespace is `require_version = true`, so a set MUST
            // carry `if_version`. Read the row first: this both
            // surfaces "no such domain" cleanly and gives the version
            // for an honest read-modify-write (a concurrent writer
            // between the get and the set is rejected with
            // version_mismatch rather than silently clobbered).
            let mut get_headers = BTreeMap::new();
            get_headers.insert("namespace".to_string(), NAMESPACE.to_string());
            get_headers.insert("key".to_string(), fqdn.clone());
            let (rc, get_body, _err_hdr) = client
                .call_with_headers_raw("maild", "maild.props.get", &get_headers, "")
                .await?;
            if rc != 0 {
                let parsed: serde_json::Value =
                    serde_json::from_str(&get_body).unwrap_or(serde_json::Value::Null);
                let msg = parsed
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or_else(|| {
                        if get_body.is_empty() {
                            "unknown error"
                        } else {
                            &get_body
                        }
                    });
                return Err(anyhow::anyhow!(
                    "cannot set on {fqdn}: row read failed (rc={rc}): {msg}"
                ));
            }
            let current: serde_json::Value =
                serde_json::from_str(&get_body).unwrap_or(serde_json::Value::Null);
            let version = current
                .get("version")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| anyhow::anyhow!("row for {fqdn} carries no version field"))?;

            // JSON-first parse: `null`/booleans/numbers/lists arrive
            // typed; anything unparsable is a plain string, so the
            // common `domain set x.com helo_identity mail.x.com`
            // needs no shell-level JSON quoting.
            let json_value: serde_json::Value =
                serde_json::from_str(&value).unwrap_or(serde_json::Value::String(value.clone()));
            let mut obj = serde_json::Map::new();
            obj.insert(field.clone(), json_value);
            let body_str = serde_json::Value::Object(obj).to_string();

            let mut headers = BTreeMap::new();
            headers.insert("namespace".to_string(), NAMESPACE.to_string());
            headers.insert("key".to_string(), fqdn.clone());
            headers.insert("merge".to_string(), "patch".to_string());
            headers.insert("if_version".to_string(), version.to_string());
            let (rc, resp_body, _err_hdr) = client
                .call_with_headers_raw("maild", "maild.props.set", &headers, &body_str)
                .await?;
            if rc == 0 {
                println!("Set {field} = {value} on {fqdn}");
            } else {
                let parsed: serde_json::Value =
                    serde_json::from_str(&resp_body).unwrap_or(serde_json::Value::Null);
                let msg = parsed
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or_else(|| {
                        if resp_body.is_empty() {
                            "unknown error"
                        } else {
                            &resp_body
                        }
                    });
                return Err(anyhow::anyhow!("set failed (rc={rc}): {msg}"));
            }
        }
        DomainAction::List => {
            let mut headers = BTreeMap::new();
            headers.insert("namespace".to_string(), NAMESPACE.to_string());
            let (rc, body_str, _err_hdr) = client
                .call_with_headers_raw("maild", "maild.props.list", &headers, "")
                .await?;
            if rc != 0 {
                let parsed: serde_json::Value =
                    serde_json::from_str(&body_str).unwrap_or(serde_json::Value::Null);
                let msg = parsed
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or_else(|| {
                        if body_str.is_empty() {
                            "unknown error"
                        } else {
                            &body_str
                        }
                    });
                return Err(anyhow::anyhow!("list failed (rc={rc}): {msg}"));
            }
            let resp: serde_json::Value =
                serde_json::from_str(&body_str).unwrap_or(serde_json::Value::Null);
            let records = resp
                .get("records")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            if records.is_empty() {
                println!("No domains.");
            } else {
                println!("{:<40} {:<16} Enabled", "FQDN", "Role");
                println!("{}", "-".repeat(70));
                for r in records {
                    let fqdn = r.get("key").and_then(|v| v.as_str()).unwrap_or("");
                    let role = r
                        .get("fields")
                        .and_then(|f| f.get("role"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let enabled = r
                        .get("fields")
                        .and_then(|f| f.get("enabled"))
                        .and_then(|v| v.as_bool())
                        .map(|b| if b { "yes" } else { "no" })
                        .unwrap_or("?");
                    println!("{fqdn:<40} {role:<16} {enabled}");
                }
            }
        }
        DomainAction::Show { fqdn } => {
            let mut headers = BTreeMap::new();
            headers.insert("namespace".to_string(), NAMESPACE.to_string());
            headers.insert("key".to_string(), fqdn.clone());
            let (rc, body_str, _err_hdr) = client
                .call_with_headers_raw("maild", "maild.props.get", &headers, "")
                .await?;
            if rc == 0 {
                // Print pretty JSON of the full record so operators
                // can read every schema field. The substrate-shaped
                // wrapper (`fields`, `version`, …) is kept verbatim;
                // a future `--format=table` flag could be added on
                // top of this without breaking existing consumers.
                let resp: serde_json::Value =
                    serde_json::from_str(&body_str).unwrap_or(serde_json::Value::Null);
                let pretty =
                    serde_json::to_string_pretty(&resp).unwrap_or_else(|_| body_str.clone());
                println!("{pretty}");
            } else {
                let parsed: serde_json::Value =
                    serde_json::from_str(&body_str).unwrap_or(serde_json::Value::Null);
                let code = parsed.get("error_code").and_then(|v| v.as_str());
                if code == Some("not_found") {
                    println!("Domain {fqdn} not found");
                } else {
                    let msg = parsed
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or_else(|| {
                            if body_str.is_empty() {
                                "unknown error"
                            } else {
                                &body_str
                            }
                        });
                    return Err(anyhow::anyhow!("show failed (rc={rc}): {msg}"));
                }
            }
        }
        DomainAction::Remove { fqdn } => {
            // `maild.domains` runs with `require_version = true`
            // (props/domains.rs:492 — closes the hook/commit TOCTOU
            // window). The substrate enforces `if_version` on delete
            // BEFORE checking row existence (lib-props runtime.rs:405),
            // so a missing `if_version` header returns a validation
            // error, NOT `not_found`. Pre-get to either prove the row
            // is absent (idempotent success) or capture its current
            // version for the conditional delete.
            let mut get_headers = BTreeMap::new();
            get_headers.insert("namespace".to_string(), NAMESPACE.to_string());
            get_headers.insert("key".to_string(), fqdn.clone());
            let (get_rc, get_body, _get_err) = client
                .call_with_headers_raw("maild", "maild.props.get", &get_headers, "")
                .await?;
            if get_rc != 0 {
                let parsed: serde_json::Value =
                    serde_json::from_str(&get_body).unwrap_or(serde_json::Value::Null);
                let code = parsed.get("error_code").and_then(|v| v.as_str());
                if code == Some("not_found") {
                    println!("Domain {fqdn} not found");
                    return Ok(());
                }
                let msg = parsed
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or_else(|| {
                        if get_body.is_empty() {
                            "unknown error"
                        } else {
                            &get_body
                        }
                    });
                return Err(anyhow::anyhow!(
                    "remove failed during pre-get (rc={get_rc}): {msg}"
                ));
            }
            let get_resp: serde_json::Value =
                serde_json::from_str(&get_body).unwrap_or(serde_json::Value::Null);
            // Substrate's `Version(pub u64)` (record.rs:21) is
            // unsigned; use `as_u64` to mirror the wire type.
            let version = get_resp
                .get("version")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| {
                    anyhow::anyhow!("remove failed: pre-get response missing `version` field")
                })?;

            let mut headers = BTreeMap::new();
            headers.insert("namespace".to_string(), NAMESPACE.to_string());
            headers.insert("key".to_string(), fqdn.clone());
            headers.insert("if_version".to_string(), version.to_string());
            let (rc, body_str, _err_hdr) = client
                .call_with_headers_raw("maild", "maild.props.delete", &headers, "")
                .await?;
            if rc == 0 {
                println!("Removed domain {fqdn}");
            } else {
                let parsed: serde_json::Value =
                    serde_json::from_str(&body_str).unwrap_or(serde_json::Value::Null);
                let code = parsed.get("error_code").and_then(|v| v.as_str());
                let msg = parsed
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or_else(|| {
                        if body_str.is_empty() {
                            "unknown error"
                        } else {
                            &body_str
                        }
                    });
                if code == Some("version_mismatch") {
                    return Err(anyhow::anyhow!(
                        "remove failed: row changed concurrently between pre-get \
                         and delete ({msg}). Retry the command."
                    ));
                }
                if code == Some("not_found") {
                    // Pre-get saw the row, but it vanished before
                    // delete — treat as idempotent success.
                    println!("Domain {fqdn} not found");
                    return Ok(());
                }
                return Err(anyhow::anyhow!("remove failed (rc={rc}): {msg}"));
            }
        }
    }
    Ok(())
}

/// Run a CLI `tls-identity` subcommand as a transient Bus client.
/// Read-only against `maild.tls_identities`: the namespace's
/// `auth_policy` does not grant a write cap to any peer
/// (`crates/cosmix-maild/src/props/tls_identities.rs`), and its
/// `before_set` / `before_delete` reject every operator write with
/// the `tls_identities_readonly:` prefix. Hence the CLI exposes
/// `list` + `show` only.
async fn run_tls_identity_cli(action: TlsIdentityAction) -> Result<()> {
    use std::collections::BTreeMap;
    const NAMESPACE: &str = "tls_identities";

    let client = cosmix_config::client_helpers::connect_anonymous_default()
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "maild daemon not reachable via broker ({e}). \
                 `cosmix-maild tls-identity` requires the daemon to be \
                 running — `systemctl start cosmix-maild` (or equivalent) first."
            )
        })?;

    match action {
        TlsIdentityAction::List => {
            let mut headers = BTreeMap::new();
            headers.insert("namespace".to_string(), NAMESPACE.to_string());
            let (rc, body_str, _err_hdr) = client
                .call_with_headers_raw("maild", "maild.props.list", &headers, "")
                .await?;
            if rc != 0 {
                let parsed: serde_json::Value =
                    serde_json::from_str(&body_str).unwrap_or(serde_json::Value::Null);
                let msg = parsed
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or_else(|| {
                        if body_str.is_empty() {
                            "unknown error"
                        } else {
                            &body_str
                        }
                    });
                return Err(anyhow::anyhow!("list failed (rc={rc}): {msg}"));
            }
            let resp: serde_json::Value =
                serde_json::from_str(&body_str).unwrap_or(serde_json::Value::Null);
            let records = resp
                .get("records")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            if records.is_empty() {
                println!("No TLS identities.");
            } else {
                // `default` gets its own yes/no column (it's the
                // SNI-resolver's fallback row, called out explicitly
                // for operator legibility). `no_sni_fallback` and
                // `strict_sni` are joined into a comma-separated
                // `Flags` column; absent flags are omitted rather
                // than printed as "no", so an empty `Flags` column
                // means neither bool is set.
                println!(
                    "{:<32} {:<24} {:<8} Source",
                    "Server Name", "Flags", "Default"
                );
                println!("{}", "-".repeat(80));
                for r in records {
                    let server_name = r.get("key").and_then(|v| v.as_str()).unwrap_or("");
                    let fields = r.get("fields");
                    let bool_flag = |name: &str| -> bool {
                        fields
                            .and_then(|f| f.get(name))
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                    };
                    let default = if bool_flag("default") { "yes" } else { "no" };
                    let mut flags: Vec<&str> = Vec::new();
                    if bool_flag("no_sni_fallback") {
                        flags.push("no_sni_fallback");
                    }
                    if bool_flag("strict_sni") {
                        flags.push("strict_sni");
                    }
                    let flags_col = flags.join(",");
                    let source = fields
                        .and_then(|f| f.get("source"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    println!("{server_name:<32} {flags_col:<24} {default:<8} {source}");
                }
            }
        }
        TlsIdentityAction::Show { server_name } => {
            let mut headers = BTreeMap::new();
            headers.insert("namespace".to_string(), NAMESPACE.to_string());
            headers.insert("key".to_string(), server_name.clone());
            let (rc, body_str, _err_hdr) = client
                .call_with_headers_raw("maild", "maild.props.get", &headers, "")
                .await?;
            if rc == 0 {
                let resp: serde_json::Value =
                    serde_json::from_str(&body_str).unwrap_or(serde_json::Value::Null);
                let pretty =
                    serde_json::to_string_pretty(&resp).unwrap_or_else(|_| body_str.clone());
                println!("{pretty}");
            } else {
                let parsed: serde_json::Value =
                    serde_json::from_str(&body_str).unwrap_or(serde_json::Value::Null);
                let code = parsed.get("error_code").and_then(|v| v.as_str());
                if code == Some("not_found") {
                    println!("TLS identity {server_name} not found");
                } else {
                    let msg = parsed
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or_else(|| {
                            if body_str.is_empty() {
                                "unknown error"
                            } else {
                                &body_str
                            }
                        });
                    return Err(anyhow::anyhow!("show failed (rc={rc}): {msg}"));
                }
            }
        }
    }
    Ok(())
}

/// Run a CLI `engine-config` subcommand as a transient Bus client.
/// Only `show` is exposed; `Replace` requires the full 12-field
/// body, and this CLI intentionally leaves both `Replace` and
/// `Patch` to ops scripts that own mutation shape/versioning (see
/// the `EngineConfig` `Command` docstring for the rationale).
async fn run_engine_config_cli(action: EngineConfigAction) -> Result<()> {
    use std::collections::BTreeMap;
    const NAMESPACE: &str = "engine_config";

    let client = cosmix_config::client_helpers::connect_anonymous_default()
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "maild daemon not reachable via broker ({e}). \
                 `cosmix-maild engine-config` requires the daemon to \
                 be running — `systemctl start cosmix-maild` (or \
                 equivalent) first."
            )
        })?;

    match action {
        EngineConfigAction::Show => {
            // Singleton namespace: omit the `key` header. Bus-side
            // `build_record_key` (`crates/cosmix-lib-props-store/src/bus/
            // mutation.rs`) accepts either an absent or empty-string
            // key for Singleton cardinality; a non-empty key would
            // be rejected with `singleton namespace: key header must
            // be empty`.
            let mut headers = BTreeMap::new();
            headers.insert("namespace".to_string(), NAMESPACE.to_string());
            let (rc, body_str, _err_hdr) = client
                .call_with_headers_raw("maild", "maild.props.get", &headers, "")
                .await?;
            if rc == 0 {
                match serde_json::from_str::<serde_json::Value>(&body_str) {
                    Ok(resp) => {
                        let pretty = serde_json::to_string_pretty(&resp)
                            .unwrap_or_else(|_| body_str.clone());
                        println!("{pretty}");
                    }
                    Err(_) => println!("{body_str}"),
                }
            } else {
                let parsed: serde_json::Value =
                    serde_json::from_str(&body_str).unwrap_or(serde_json::Value::Null);
                let code = parsed.get("error_code").and_then(|v| v.as_str());
                let msg = parsed
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or_else(|| {
                        if body_str.is_empty() {
                            "unknown error"
                        } else {
                            &body_str
                        }
                    });
                let detail = match code {
                    Some(c) => format!("{c}: {msg}"),
                    None => msg.to_string(),
                };
                return Err(anyhow::anyhow!("show failed (rc={rc}): {detail}"));
            }
        }
    }
    Ok(())
}

/// Run a CLI `dkim` subcommand as a transient Bus client. Each
/// arm sends the verb args as a JSON request body (the daemon's
/// `resolve_args` reads body OR `args:` header — body is simpler
/// to construct). DKIM verbs emit errors as `{"error": "..."}`
/// (pre-`error_code` envelope; see `bus/dkim.rs:128-130`), and the
/// renderer falls back through `error` → `message` → raw body for
/// forward compatibility if the envelope is ever migrated.
async fn run_dkim_cli(action: DkimAction) -> Result<()> {
    use std::collections::BTreeMap;

    let client = cosmix_config::client_helpers::connect_anonymous_default()
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "maild daemon not reachable via broker ({e}). \
                 `cosmix-maild dkim` requires the daemon to be \
                 running — `systemctl start cosmix-maild` (or \
                 equivalent) first."
            )
        })?;

    let (verb, body) = match action {
        DkimAction::Generate {
            domain,
            selector,
            algorithm,
        } => (
            "maild.dkim.generate",
            serde_json::json!({
                "domain": domain,
                "selector": selector,
                "algorithm": algorithm,
            })
            .to_string(),
        ),
        DkimAction::Rotate { domain, selector } => (
            "maild.dkim.rotate",
            serde_json::json!({
                "domain": domain,
                "selector": selector,
            })
            .to_string(),
        ),
        DkimAction::Retire { domain, selector } => (
            "maild.dkim.retire",
            serde_json::json!({
                "domain": domain,
                "selector": selector,
            })
            .to_string(),
        ),
    };

    let (rc, body_str, _err_hdr) = client
        .call_with_headers_raw("maild", verb, &BTreeMap::new(), &body)
        .await?;
    if rc == 0 {
        match serde_json::from_str::<serde_json::Value>(&body_str) {
            Ok(resp) => {
                let pretty =
                    serde_json::to_string_pretty(&resp).unwrap_or_else(|_| body_str.clone());
                println!("{pretty}");
            }
            Err(_) => println!("{body_str}"),
        }
    } else {
        let parsed: serde_json::Value =
            serde_json::from_str(&body_str).unwrap_or(serde_json::Value::Null);
        // DKIM verbs report errors as `{"error": "..."}` (pre-`error_code`
        // envelope); fall back to `message` if a future migration adds it.
        let msg = parsed
            .get("error")
            .and_then(|v| v.as_str())
            .or_else(|| parsed.get("message").and_then(|v| v.as_str()))
            .unwrap_or_else(|| {
                if body_str.is_empty() {
                    "unknown error"
                } else {
                    &body_str
                }
            });
        return Err(anyhow::anyhow!("{verb} failed (rc={rc}): {msg}"));
    }
    Ok(())
}

/// Shared CLI dispatch for operator-facing maild verbs that share
/// four invariants:
///
/// 1. Called as a transient anonymous Bus client (no caller cap).
/// 2. Optional body — rules-stats may pass `{"top_n": N}`,
///    bayesian-stats passes `{"account_id": "..."}`, while
///    rules-reload and tls-reload pass `""`. Callers pass `""`
///    for no-body verbs; the `NodedClient` omits the Bus body
///    field entirely in that case (see
///    `cosmix-lib-client/src/native.rs` `call_with_headers_raw`),
///    which the daemon's arg resolver treats the same as `Null`.
/// 3. No custom headers.
/// 4. Pre-SPEC-12 error envelope `{"error": "..."}` on `rc != 0`
///    (not the SPEC 12 `{"error_code", "message"}` envelope). The
///    renderer falls back `error` → `message` → raw body so a
///    future envelope migration is forward-compatible.
///
/// What the helper does NOT promise: that callers have no
/// persistent side effects. `maild.tls.reload` reprojects
/// `maild.tls_identities` through `Runtime` (durable SqliteStore
/// writes) before swapping the resolver; the resolver-swap path
/// is fail-loud-and-keep, but the projection can be partially
/// stale on a mid-apply storage failure (see
/// `props/tls_identities.rs` projection-apply comments). Helper
/// shape ≠ verb-level safety guarantee.
async fn run_inspection_verb_cli(
    daemon_keyword: &'static str,
    verb: &'static str,
    body: &str,
) -> Result<()> {
    use std::collections::BTreeMap;

    let client = cosmix_config::client_helpers::connect_anonymous_default()
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "maild daemon not reachable via broker ({e}). \
                 `cosmix-maild {daemon_keyword}` requires the daemon to \
                 be running — `systemctl start cosmix-maild` (or \
                 equivalent) first."
            )
        })?;

    let headers: BTreeMap<String, String> = BTreeMap::new();
    let (rc, body_str, _err_hdr) = client
        .call_with_headers_raw("maild", verb, &headers, body)
        .await?;
    if rc == 0 {
        match serde_json::from_str::<serde_json::Value>(&body_str) {
            Ok(resp) => {
                let pretty =
                    serde_json::to_string_pretty(&resp).unwrap_or_else(|_| body_str.clone());
                println!("{pretty}");
            }
            Err(_) => println!("{body_str}"),
        }
    } else {
        let parsed: serde_json::Value =
            serde_json::from_str(&body_str).unwrap_or(serde_json::Value::Null);
        let msg = parsed
            .get("error")
            .and_then(|v| v.as_str())
            .or_else(|| parsed.get("message").and_then(|v| v.as_str()))
            .unwrap_or_else(|| {
                if body_str.is_empty() {
                    "unknown error"
                } else {
                    &body_str
                }
            });
        return Err(anyhow::anyhow!("{verb} failed (rc={rc}): {msg}"));
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls CryptoProvider");

    let cli = Cli::parse();

    // Tracing: daemon (`serve`) gets the rolling file appender that
    // wants a writable `$HOME/.local/log/cosmix`; one-shot CLI
    // subcommands (`account`, `migrate`, `queue`) write only to
    // stderr. This is what lets `markc` run `cosmix-maild account
    // list` without `sudo` or a `HOME=…` override — the file
    // appender would otherwise force one or the other on a real
    // shared-config deployment. The guard is held only on the
    // daemon path; CLI runs return before `main` ends, so stderr
    // flushes naturally.
    // Serve gets daemon-class journald logging with the live
    // `maild.log` substrate watcher (attached after `build_runtime`);
    // CLI runs get stderr-only at info (matching the old
    // `init_tracing_stderr` behaviour) so `cosmix-maild account list`
    // works without `sudo`/`HOME=…`. The handle MUST outlive the
    // process — held in `log_handle` for the whole of `main`.
    let log_handle = match &cli.command {
        Command::Serve => cosmix_log::init(
            &cosmix_log::LogOpts::default(),
            &cosmix_log::StatsOpts::default(),
            cosmix_log::LogDefaults::daemon("cosmix-maild").with_stats(false),
        )
        .expect("logging init failed"),
        _ => cosmix_log::init(
            &cosmix_log::LogOpts::default(),
            &cosmix_log::StatsOpts::default(),
            cosmix_log::LogDefaults::gui("cosmix-maild").with_level(cosmix_log::LogLevel::Info),
        )
        .expect("logging init failed"),
    };

    // Config search order (first match wins):
    //   1. `--config <path>` from the command line
    //   2. `/etc/cosmix/maild/config.conf.mix` — the system-wide config
    //      the systemd unit reads. Putting this *ahead* of the XDG user
    //      path means a non-root caller (e.g. `markc` running
    //      `cosmix-maild account list`) resolves to the daemon's actual
    //      database / MDS / account roster automatically. Only `NotFound`
    //      falls through; permission-denied / IO error / parse error all
    //      hard-fail with a clear message so a typo or wrong perms can't
    //      silently re-route the caller to a different DB.
    //   3. XDG user path (`$HOME/.config/cosmix/jmap.conf.mix`) — per-user
    //      / dev installs.
    //   4. node-config-derived defaults (mesh-aware config).
    //   5. compile-time defaults.
    let cfg = if let Some(path) = &cli.config {
        config::Config::load(path)?
    } else {
        let candidates = [
            config::Config::system_config_path(),
            config::Config::config_path(),
        ];
        let mut found = None;
        for path in &candidates {
            if let Some(c) = config::Config::try_load_optional(path)? {
                found = Some(c);
                break;
            }
        }
        if let Some(c) = found {
            c
        } else if let Some(nc) = cosmix_config::node::load_node_config()? {
            let c = config::Config::from(&nc);
            c.validate_tls()
                .map_err(|e| anyhow::anyhow!("invalid maild.tls.identity in node config: {e}"))?;
            c
        } else {
            config::Config::default()
        }
    };

    match cli.command {
        Command::Migrate => {
            let database = db::Db::connect(&cfg.database_path, &cfg.blob_dir).await?;
            database.migrate().await?;
            println!("Migrations applied successfully.");
        }

        Command::Account { action } => {
            // Bus-only client path — no local DB or MDS access.
            // Daemon must be running on the broker. This makes
            // `cosmix-maild account *` operable from any peer in
            // the WG `/24` mesh, not just the daemon's host.
            run_account_cli(action).await?;
        }

        Command::AccountOverrides { action } => {
            run_account_overrides_cli(action).await?;
        }

        Command::Alias { action } => {
            run_alias_cli(action).await?;
        }
        Command::Domain { action } => {
            // Bus-only client path — same posture as `account` /
            // `account-overrides`. Daemon must be running on the broker.
            run_domain_cli(action).await?;
        }

        Command::TlsIdentity { action } => {
            run_tls_identity_cli(action).await?;
        }

        Command::EngineConfig { action } => {
            run_engine_config_cli(action).await?;
        }

        Command::Dkim { action } => {
            run_dkim_cli(action).await?;
        }

        Command::Rules { action } => match action {
            RulesAction::Stats { top_n } => {
                let body = match top_n {
                    Some(n) => serde_json::json!({ "top_n": n }).to_string(),
                    None => String::new(),
                };
                run_inspection_verb_cli("rules stats", "maild.rules.stats", &body).await?;
            }
            RulesAction::Reload => {
                run_inspection_verb_cli("rules reload", "maild.rules.reload", "").await?;
            }
        },

        Command::Bayesian { action } => {
            let BayesianAction::Stats { account_id } = action;
            let body = serde_json::json!({ "account_id": account_id }).to_string();
            run_inspection_verb_cli("bayesian stats", "maild.bayesian.stats", &body).await?;
        }

        Command::Tls { action } => {
            let TlsAction::Reload = action;
            // `maild.tls.reload` takes no arguments and returns a
            // structured reload-report body (merged identities +
            // per-identity errors if any). The helper applies
            // cleanly: empty body, no custom headers, JSON
            // pretty-print, `{"error": ...}` failure envelope.
            // Verb-level semantics (resolver swap is
            // fail-loud-and-keep; substrate projection can be
            // partially stale on storage failure) are
            // documented at the verb handler, not here.
            run_inspection_verb_cli("tls reload", "maild.tls.reload", "").await?;
        }

        Command::Queue { action } => match action {
            QueueAction::List => {
                let database = db::Db::connect(&cfg.database_path, &cfg.blob_dir).await?;
                let entries = smtp::queue::list(&database.conn, 50).await?;
                if entries.is_empty() {
                    println!("Queue is empty.");
                } else {
                    println!(
                        "{:<6} {:<30} {:<6} {:<20} Error",
                        "ID", "From", "Tries", "Next Retry"
                    );
                    println!("{}", "-".repeat(90));
                    for e in entries {
                        println!(
                            "{:<6} {:<30} {:<6} {:<20} {}",
                            e.id,
                            e.from_addr,
                            e.attempts,
                            e.next_retry,
                            e.last_error.unwrap_or_default()
                        );
                    }
                }
            }
            QueueAction::Flush => {
                let database = db::Db::connect(&cfg.database_path, &cfg.blob_dir).await?;
                let count = smtp::queue::flush(&database.conn).await?;
                println!("Flushed {count} queue entries for immediate retry.");
            }
        },

        Command::Serve => {
            let built = runtime::build_runtime(&cfg, runtime::RuntimeOpts::default()).await?;
            // Live `maild.log` watcher — swaps the EnvFilter on every
            // `props.set maild.log {...}`. Spawns a task on the live
            // tokio runtime; the `log_handle` was created above.
            cosmix_log_props::attach_props(&log_handle, built.log_runtime()).await?;
            for addr in &built.smtp_handle.inbound_addrs {
                tracing::info!(addr = %addr, "SMTP inbound bound");
            }
            for addr in &built.smtp_handle.smtps_addrs {
                tracing::info!(addr = %addr, "SMTPS submission bound");
            }
            let listener = tokio::net::TcpListener::bind(&cfg.listen).await?;
            tracing::info!(addr = %cfg.listen, "cosmix-jmap JMAP listening");

            // Use TLS if cert/key configured
            if let (Some(cert_path), Some(key_path)) = (&cfg.tls_cert, &cfg.tls_key) {
                let tls_acceptor = cosmix_daemon::load_tls_config(cert_path, key_path)?;
                tracing::info!("JMAP HTTPS enabled");
                let app = built.router.clone();
                let _keepalive = built;
                loop {
                    let (stream, _peer) = listener.accept().await?;
                    let acceptor = tls_acceptor.clone();
                    let app = app.clone();
                    tokio::spawn(async move {
                        match acceptor.accept(stream).await {
                            Ok(tls_stream) => {
                                let io = hyper_util::rt::TokioIo::new(tls_stream);
                                let service = hyper_util::service::TowerToHyperService::new(app);
                                if let Err(e) = hyper_util::server::conn::auto::Builder::new(
                                    hyper_util::rt::TokioExecutor::new(),
                                )
                                .serve_connection(io, service)
                                .await
                                {
                                    tracing::debug!(error = %e, "HTTPS connection error");
                                }
                            }
                            Err(e) => {
                                tracing::debug!(error = %e, "TLS handshake failed");
                            }
                        }
                    });
                }
            } else {
                let router = built.router.clone();
                let _keepalive = built;
                axum::serve(listener, router).await?;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod cli_parser_tests {
    //! Clap-parser shape tests for the operator-facing subcommands
    //! added in the Phase 8e operator-CLI sweep (`account show`,
    //! `dkim {generate,rotate,retire}`, `engine-config show`). Each
    //! test exercises `Cli::try_parse_from` with no daemon or Bus
    //! plumbing — the goal is to lock the public argv shape so a
    //! later rename / required-flag drift surfaces as a test failure
    //! rather than as a runtime "no such argument" from operators.
    //!
    //! Happy-path parses also bind the parsed value via `matches!`
    //! so the test will break loudly if a field is renamed, not
    //! just if the parser shape changes.
    use super::*;

    fn parse(argv: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(argv)
    }

    // --- account show --------------------------------------------------

    #[test]
    fn account_show_parses_with_email() {
        let cli = parse(&["cosmix-maild", "account", "show", "alice@example.com"])
            .expect("account show <email> should parse");
        match cli.command {
            Command::Account {
                action: AccountAction::Show { email },
            } => {
                assert_eq!(email, "alice@example.com");
            }
            other => panic!(
                "expected Account::Show, got {other:?}",
                other = std::any::type_name_of_val(&other)
            ),
        }
    }

    #[test]
    fn account_show_without_email_errors() {
        let Err(err) = parse(&["cosmix-maild", "account", "show"]) else {
            panic!("account show requires <email>")
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    // --- dkim generate / rotate / retire ------------------------------

    #[test]
    fn dkim_generate_parses_with_default_algorithm() {
        let cli = parse(&[
            "cosmix-maild",
            "dkim",
            "generate",
            "--domain",
            "example.com",
            "--selector",
            "s2026a",
        ])
        .expect("dkim generate with defaults should parse");
        match cli.command {
            Command::Dkim {
                action:
                    DkimAction::Generate {
                        domain,
                        selector,
                        algorithm,
                    },
            } => {
                assert_eq!(domain, "example.com");
                assert_eq!(selector, "s2026a");
                // Default algorithm must remain `rsa-sha256` — operator
                // playbooks bake this in.
                assert_eq!(algorithm, "rsa-sha256");
            }
            _ => panic!("expected Dkim::Generate"),
        }
    }

    #[test]
    fn dkim_generate_honours_algorithm_override() {
        let cli = parse(&[
            "cosmix-maild",
            "dkim",
            "generate",
            "--domain",
            "example.com",
            "--selector",
            "s2026b",
            "--algorithm",
            "ed25519-sha256",
        ])
        .expect("--algorithm should accept ed25519-sha256");
        match cli.command {
            Command::Dkim {
                action: DkimAction::Generate { algorithm, .. },
            } => {
                assert_eq!(algorithm, "ed25519-sha256");
            }
            _ => panic!("expected Dkim::Generate"),
        }
    }

    #[test]
    fn dkim_generate_without_required_flags_errors() {
        let Err(err) = parse(&["cosmix-maild", "dkim", "generate", "--domain", "x.com"]) else {
            panic!("--selector is required")
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);

        let Err(err) = parse(&["cosmix-maild", "dkim", "generate", "--selector", "s1"]) else {
            panic!("--domain is required")
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn dkim_rotate_parses() {
        let cli = parse(&[
            "cosmix-maild",
            "dkim",
            "rotate",
            "--domain",
            "example.com",
            "--selector",
            "s2026a",
        ])
        .expect("dkim rotate should parse");
        match cli.command {
            Command::Dkim {
                action: DkimAction::Rotate { domain, selector },
            } => {
                assert_eq!(domain, "example.com");
                assert_eq!(selector, "s2026a");
            }
            _ => panic!("expected Dkim::Rotate"),
        }
    }

    #[test]
    fn dkim_rotate_without_required_flags_errors() {
        let Err(err) = parse(&["cosmix-maild", "dkim", "rotate", "--domain", "x.com"]) else {
            panic!("--selector is required")
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);

        let Err(err) = parse(&["cosmix-maild", "dkim", "rotate", "--selector", "s1"]) else {
            panic!("--domain is required")
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn dkim_retire_parses() {
        let cli = parse(&[
            "cosmix-maild",
            "dkim",
            "retire",
            "--domain",
            "example.com",
            "--selector",
            "s2026a",
        ])
        .expect("dkim retire should parse");
        match cli.command {
            Command::Dkim {
                action: DkimAction::Retire { domain, selector },
            } => {
                assert_eq!(domain, "example.com");
                assert_eq!(selector, "s2026a");
            }
            _ => panic!("expected Dkim::Retire"),
        }
    }

    #[test]
    fn dkim_retire_without_required_flags_errors() {
        let Err(err) = parse(&["cosmix-maild", "dkim", "retire", "--domain", "x.com"]) else {
            panic!("--selector is required")
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);

        let Err(err) = parse(&["cosmix-maild", "dkim", "retire", "--selector", "s1"]) else {
            panic!("--domain is required")
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    // --- engine-config show --------------------------------------------

    #[test]
    fn engine_config_show_parses() {
        let cli = parse(&["cosmix-maild", "engine-config", "show"])
            .expect("engine-config show should parse");
        assert!(matches!(
            cli.command,
            Command::EngineConfig {
                action: EngineConfigAction::Show
            }
        ));
    }

    // --- rules stats ---------------------------------------------------

    #[test]
    fn rules_stats_parses_with_default_top_n() {
        let cli =
            parse(&["cosmix-maild", "rules", "stats"]).expect("rules stats should parse bare");
        match cli.command {
            Command::Rules {
                action: RulesAction::Stats { top_n },
            } => {
                assert!(top_n.is_none(), "bare invocation must leave top_n None");
            }
            _ => panic!("expected Rules::Stats"),
        }
    }

    #[test]
    fn rules_stats_parses_explicit_top_n() {
        let cli = parse(&["cosmix-maild", "rules", "stats", "--top-n", "16"])
            .expect("rules stats --top-n 16 should parse");
        match cli.command {
            Command::Rules {
                action: RulesAction::Stats { top_n },
            } => {
                assert_eq!(top_n, Some(16));
            }
            _ => panic!("expected Rules::Stats"),
        }
    }

    #[test]
    fn rules_stats_parses_zero_top_n() {
        // `--top-n 0` is the documented cardinality-only fast path.
        // Clap's u64 parser must accept it (no implicit positivity
        // constraint creeping in via a future `value_parser` change).
        let cli = parse(&["cosmix-maild", "rules", "stats", "--top-n", "0"])
            .expect("rules stats --top-n 0 should parse");
        match cli.command {
            Command::Rules {
                action: RulesAction::Stats { top_n },
            } => {
                assert_eq!(top_n, Some(0));
            }
            _ => panic!("expected Rules::Stats"),
        }
    }

    #[test]
    fn rules_stats_rejects_non_numeric_top_n() {
        let Err(err) = parse(&["cosmix-maild", "rules", "stats", "--top-n", "lots"]) else {
            panic!("--top-n must reject non-numeric values")
        };
        // Clap routes parser failures (u64::from_str) through ValueValidation.
        assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
    }

    // --- rules reload --------------------------------------------------

    #[test]
    fn rules_reload_parses() {
        let cli = parse(&["cosmix-maild", "rules", "reload"]).expect("rules reload should parse");
        assert!(matches!(
            cli.command,
            Command::Rules {
                action: RulesAction::Reload
            }
        ));
    }

    #[test]
    fn rules_reload_rejects_unknown_positional() {
        // `rules reload` is parameter-less — same shape as
        // `tls reload`. Extra positional must surface as
        // UnknownArgument so a future commit can't silently add an
        // arg without a test gate.
        let Err(err) = parse(&["cosmix-maild", "rules", "reload", "stray"]) else {
            panic!("rules reload must reject stray positionals")
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    // --- bayesian stats ------------------------------------------------

    #[test]
    fn bayesian_stats_parses_with_account_id() {
        let cli = parse(&["cosmix-maild", "bayesian", "stats", "42"])
            .expect("bayesian stats <account-id> should parse");
        match cli.command {
            Command::Bayesian {
                action: BayesianAction::Stats { account_id },
            } => {
                assert_eq!(account_id, "42");
            }
            _ => panic!("expected Bayesian::Stats"),
        }
    }

    #[test]
    fn bayesian_stats_without_account_id_errors() {
        let Err(err) = parse(&["cosmix-maild", "bayesian", "stats"]) else {
            panic!("bayesian stats requires <account-id>")
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn rules_without_action_errors() {
        // `rules` alone is invalid — clap must refuse to fill in
        // either `stats` or `reload` implicitly. Symmetry with
        // the webd inspection CLI which pinned the same shape.
        let Err(err) = parse(&["cosmix-maild", "rules"]) else {
            panic!("`rules` requires a sub-action")
        };
        // Clap reports this as MissingSubcommand for a
        // `#[command(subcommand)]` arm that has no
        // `SubcommandRequired = false` override.
        assert!(matches!(
            err.kind(),
            clap::error::ErrorKind::MissingSubcommand
                | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        ));
    }

    #[test]
    fn bayesian_without_action_errors() {
        let Err(err) = parse(&["cosmix-maild", "bayesian"]) else {
            panic!("`bayesian` requires a sub-action")
        };
        assert!(matches!(
            err.kind(),
            clap::error::ErrorKind::MissingSubcommand
                | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        ));
    }

    // --- tls reload ----------------------------------------------------

    #[test]
    fn tls_reload_parses() {
        let cli = parse(&["cosmix-maild", "tls", "reload"]).expect("tls reload should parse");
        assert!(matches!(
            cli.command,
            Command::Tls {
                action: TlsAction::Reload
            }
        ));
    }

    #[test]
    fn tls_without_action_errors() {
        // `tls` alone is invalid — `reload` is the only action and
        // clap must refuse to fill it in implicitly. Symmetric to
        // the webd `tls` and the maild `rules` / `bayesian` shapes.
        let Err(err) = parse(&["cosmix-maild", "tls"]) else {
            panic!("`tls` requires a sub-action")
        };
        assert!(matches!(
            err.kind(),
            clap::error::ErrorKind::MissingSubcommand
                | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        ));
    }

    #[test]
    fn tls_reload_rejects_unknown_positional() {
        // `tls reload` takes no arguments — an extra positional
        // must be a parse error, not silently accepted (which would
        // be the failure mode if a future commit added an optional
        // arg without a test gate).
        let Err(err) = parse(&["cosmix-maild", "tls", "reload", "stray"]) else {
            panic!("tls reload must reject stray positionals")
        };
        // Clap reports unknown positionals via UnknownArgument.
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn bayesian_stats_accepts_string_account_id_verbatim() {
        // CLI doesn't pre-validate digit-only — the daemon's
        // `parse_account_id` (bus/bayesian.rs) is the trust boundary.
        // What this test pins is that the parser passes the value
        // through verbatim so the daemon sees exactly what the
        // operator typed; that keeps the daemon's error message
        // (`account_id must be a non-negative integer, got ...`)
        // truthful instead of mangled.
        let cli = parse(&["cosmix-maild", "bayesian", "stats", "not-a-number"])
            .expect("bayesian stats must accept the value at parse time");
        match cli.command {
            Command::Bayesian {
                action: BayesianAction::Stats { account_id },
            } => {
                assert_eq!(account_id, "not-a-number");
            }
            _ => panic!("expected Bayesian::Stats"),
        }
    }
}
