//! `cosmix-mesh-sign` — SPEC 13 1b-b: sign the mesh inventory.
//!
//! Turns the Phase-1a *unsigned* authored inventory into the signed
//! trust root (§7.1). It reads the one authored `inventory.mix` through
//! the **same** strict-data parser the Mix tooling uses
//! (`cosmix_config::load_mix_data`), builds the §7.1 signed payload,
//! signs its canonical bytes with the genesis Ed25519 key (stored in the
//! operator secrets DB), and emits `inventory.signed`. The signer shares
//! the one authoritative canonicaliser + verify logic with the verifier
//! (`noded`, 1b-c) via `cosmix_mesh_trust`, so signer and verifier
//! cannot disagree on the bytes a signature covers (§6, the cardinal
//! risk).
//!
//! Subcommands:
//! - `genesis` — generate the genesis keypair + store the signing key in
//!   the secrets DB (one-time mesh trust-root ceremony).
//! - `sign` — inventory.mix → inventory.signed (and self-verify).
//! - `verify` — check an inventory.signed against the genesis key.
//!
//! Testbed custody (§6.4, loosened 2026-06-03): the genesis signing key
//! lives **online** in `~/.ns/_etc/secrets/secrets.db`. Offline-medium /
//! m-of-n custody is the production tightening.

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::{Duration, SecondsFormat, Utc};
use clap::{Parser, Subcommand};
use cosmix_config::Value;
use cosmix_mesh_trust::inventory::{
    ALG_ED25519, CANONICAL_ENCODING_V1, InvSignature, InventoryPayload, KeyStatus, NodeTrustState,
    SIGNER_OWNED_FIELDS, SignedInventory, TrustedKey, authoring_blake3_for_value,
};
use cosmix_mesh_trust::routing::{RoutingMember, strict_routing_view};
use ed25519_dalek::{Signer, SigningKey};
use rand::rngs::OsRng;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value as Json, json};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

/// The genesis key_id — the non-removable trust anchor (§7.2 rule 1).
const GENESIS_KEY_ID: &str = "genesis";
/// The `service` column under which the genesis key is filed in secrets.db.
const SECRETS_SERVICE: &str = "mesh-genesis";
/// The `vnode` sentinel for mesh-wide (not node-specific) secrets — a
/// non-NULL value so the `(vnode, domain, service, username)` UNIQUE
/// index actually applies (SQLite treats NULLs as distinct).
const SECRETS_VNODE: &str = "mesh";
/// The `service` column for a node's `kind:"d2"` admission key (§9a / SPEC 13
/// slice 2-d). Filed PER NODE: `vnode=<node>` (the secret belongs to that node),
/// `username="d2"` (the credential role) — distinct from the mesh-wide genesis.
const D2_SERVICE: &str = "mesh-d2";
/// The `username` (credential role) for the current d2 admission key.
const D2_KEY_ID: &str = "d2";
const DEFAULT_SECRETS_DB: &str = "~/.ns/_etc/secrets/secrets.db";
const VERIFY_JSON_FORMAT: &str = "cosmix-mesh-sign.verify.v1";
const ED25519_PUBLIC_KEY_LEN: usize = 32;
/// Mix represents numbers as f64, so this is the largest integer the control
/// plane can compare without rounding.
const MAX_EXACT_MIX_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Parser)]
#[command(
    name = "cosmix-mesh-sign",
    about = "SPEC 13 mesh inventory signer (sign + verify the trust root)"
)]
struct Cli {
    /// Path to the operator secrets DB (SQLite).
    #[arg(long, default_value = DEFAULT_SECRETS_DB, global = true)]
    secrets_db: String,
    /// Mesh fqdn — the `domain` column the genesis key is filed under.
    #[arg(long, default_value = "bus", global = true)]
    mesh: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Generate the genesis Ed25519 keypair and store the signing key in
    /// the secrets DB. Refuses to overwrite an existing key unless
    /// --force (rotating genesis invalidates every prior signature).
    Genesis {
        #[arg(long)]
        force: bool,
    },
    /// Sign an `inventory.mix` → `inventory.signed`, then self-verify the
    /// emitted artifact.
    Sign {
        /// The authored strict-data inventory (`inventory.mix`).
        inventory: PathBuf,
        /// Output path for the signed artifact.
        #[arg(short, long)]
        out: PathBuf,
        /// Advisory `valid_until` horizon, in days from now (§16a: a hint
        /// only, never a security gate).
        #[arg(long, default_value = "90")]
        valid_days: i64,
        /// Emit a RECOVERY inventory (§6.4): sets `recovery: true` +
        /// `recovery_generation: <N>`. A genesis-signed recovery supersedes
        /// the epoch axis (a node accepts it even at a lower epoch), so use
        /// it to re-establish control after a stolen active key seized a
        /// higher epoch. N MUST strictly exceed the node's last recovery
        /// generation.
        #[arg(
            long,
            value_name = "GENERATION",
            value_parser = parse_recovery_generation,
            conflicts_with = "recovery_generation"
        )]
        recovery: Option<u64>,
        /// Stamp the locked control-floor recovery generation on a NORMAL
        /// inventory without setting `recovery:true`. Pass that exact snapshot,
        /// never a larger value: a higher genesis-signed generation raises fleet
        /// floors.
        #[arg(
            long,
            value_name = "GENERATION",
            value_parser = parse_recovery_generation,
            conflicts_with = "recovery"
        )]
        recovery_generation: Option<u64>,
    },
    /// Verify an `inventory.signed` against the genesis verify key.
    Verify {
        signed: PathBuf,
        /// Read the trusted genesis PUBLIC key from this file instead of the
        /// secrets DB. The file contains one base64-encoded Ed25519 key.
        #[arg(long, value_name = "PATH", conflicts_with = "genesis_pub_base64")]
        genesis_pub: Option<PathBuf>,
        /// Use this base64-encoded genesis PUBLIC key instead of the secrets
        /// DB. Mutually exclusive with --genesis-pub.
        #[arg(long, value_name = "B64", conflicts_with = "genesis_pub")]
        genesis_pub_base64: Option<String>,
        /// Require the authenticated payload to name this exact mesh. This is
        /// independent of the global --mesh secrets-DB selector.
        #[arg(long, value_name = "NAME")]
        expected_mesh: Option<String>,
        /// Emit exactly one versioned JSON object on stdout.
        #[arg(long)]
        json: bool,
        /// Include the authenticated payload in --json output so a caller
        /// never needs to reopen the envelope after verification.
        #[arg(long, requires = "json")]
        include_payload: bool,
        /// Require the signed payload's authored semantic hash to match this
        /// strict-data inventory.
        #[arg(long, value_name = "PATH")]
        against_authored: Option<PathBuf>,
    },
    /// Print the genesis PUBLIC verify key (base64) — the value to
    /// provision out-of-band to each node as `/etc/cosmix/noded/genesis.pub`
    /// (§7.4). Reads the signing key from the secrets DB.
    Pubkey,
    /// Generate a node's `kind:"d2"` admission keypair (§9a / slice 2-d) and
    /// store the PRIVATE seed in the secrets DB under that node. Prints the
    /// PUBLIC key (base64) to author into the node's `credentials[]` in
    /// `inventory.mix`. Testbed custody (like genesis): the seed lives online in
    /// secrets.db; node-local generation is the production tightening. Refuses
    /// to overwrite an existing d2 key unless `--force` (rotating it invalidates
    /// that node's in-flight admission proofs).
    D2Gen {
        /// The node name the d2 key belongs to (e.g. `delta`).
        node: String,
        #[arg(long)]
        force: bool,
    },
    /// Print a node's d2 PUBLIC key (base64) from the secrets DB — the value to
    /// author into that node's `credentials[]` in `inventory.mix`.
    D2Pubkey { node: String },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let db = expand_tilde(&cli.secrets_db);
    match cli.cmd {
        Cmd::Genesis { force } => cmd_genesis(&db, &cli.mesh, force),
        Cmd::Sign {
            inventory,
            out,
            valid_days,
            recovery,
            recovery_generation,
        } => cmd_sign(
            &db,
            &cli.mesh,
            &inventory,
            &out,
            valid_days,
            recovery,
            recovery_generation,
        ),
        Cmd::Verify {
            signed,
            genesis_pub,
            genesis_pub_base64,
            expected_mesh,
            json,
            include_payload,
            against_authored,
        } => emit_verify(
            &db,
            &cli.mesh,
            &signed,
            json,
            VerifyOptions {
                genesis_pub: genesis_pub.as_deref(),
                genesis_pub_base64: genesis_pub_base64.as_deref(),
                expected_mesh: expected_mesh.as_deref(),
                include_payload,
                against_authored: against_authored.as_deref(),
            },
        ),
        Cmd::Pubkey => {
            let sk = load_signing_key(&db, &cli.mesh)?;
            // The bare base64 key, no trailing text — suitable to redirect
            // straight into genesis.pub.
            println!("{}", B64.encode(sk.verifying_key().to_bytes()));
            Ok(())
        }
        Cmd::D2Gen { node, force } => cmd_d2gen(&db, &cli.mesh, &node, force),
        Cmd::D2Pubkey { node } => cmd_d2pubkey(&db, &cli.mesh, &node),
    }
}

/// Expand a leading `~/` against `$HOME` (the secrets DB default path).
fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return Path::new(&home).join(rest);
    }
    PathBuf::from(p)
}

fn parse_recovery_generation(value: &str) -> std::result::Result<u64, String> {
    let generation = value.parse::<u64>().map_err(|_| {
        format!("invalid recovery generation {value:?}: expected an unsigned integer")
    })?;
    if generation > MAX_EXACT_MIX_INTEGER {
        return Err(format!(
            "recovery generation {generation} exceeds {MAX_EXACT_MIX_INTEGER}, the largest integer Mix can compare exactly (Mix numbers use f64; refusing a rounded freshness floor)"
        ));
    }
    Ok(generation)
}

// ---------------------------------------------------------------------
// secrets.db — the genesis signing key custody (§6.4 testbed custody)
// ---------------------------------------------------------------------

/// Fetch a base64 Ed25519 seed by its full `(vnode, domain, service, username)`
/// secrets.db key, or `None` if absent. The one row-reader genesis + d2 share.
fn fetch_seed_row(
    conn: &Connection,
    vnode: &str,
    mesh: &str,
    service: &str,
    username: &str,
) -> Result<Option<String>> {
    conn.query_row(
        "SELECT password FROM secrets \
         WHERE vnode=?1 AND domain=?2 AND service=?3 AND username=?4",
        params![vnode, mesh, service, username],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .with_context(|| format!("query {service}/{username} key"))
}

/// Store a base64 Ed25519 seed at its full secrets.db key. Explicit
/// update-or-insert (avoids ON CONFLICT vs NULL-vnode pitfalls); returns `true`
/// if it UPDATED an existing row, `false` if it inserted a new one.
fn upsert_seed(
    conn: &Connection,
    vnode: &str,
    mesh: &str,
    service: &str,
    username: &str,
    seed_b64: &str,
    notes: &str,
) -> Result<bool> {
    let updated = conn.execute(
        "UPDATE secrets SET password=?1, notes=?2, updated_at=datetime('now','localtime') \
         WHERE vnode=?3 AND domain=?4 AND service=?5 AND username=?6",
        params![seed_b64, notes, vnode, mesh, service, username],
    )?;
    if updated == 0 {
        conn.execute(
            "INSERT INTO secrets (vnode, domain, service, username, password, notes) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![vnode, mesh, service, username, seed_b64, notes],
        )?;
    }
    Ok(updated != 0)
}

/// Insert-only store of a base64 seed at its secrets.db key. A pre-existing row
/// fails on the `(vnode,domain,service,username)` UNIQUE index — which is how
/// the non-`--force` refuse stays ATOMIC (no check-then-write race).
fn insert_seed(
    conn: &Connection,
    vnode: &str,
    mesh: &str,
    service: &str,
    username: &str,
    seed_b64: &str,
    notes: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO secrets (vnode, domain, service, username, password, notes) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![vnode, mesh, service, username, seed_b64, notes],
    )?;
    Ok(())
}

/// Whether a rusqlite error is a UNIQUE/constraint violation (the row exists).
fn is_unique_violation(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(f, _)
            if f.code == rusqlite::ErrorCode::ConstraintViolation
    )
}

/// Generate an Ed25519 keypair and store its seed at the given secrets.db key,
/// ATOMICALLY honouring refuse-overwrite. `force=false`: a pre-existing row is
/// refused by the UNIQUE index in a single statement (no fetch-then-write race),
/// returning `None`; `force=true`: rotates (update-or-insert). On success returns
/// `Some((seed_b64, pub_b64, rotated))` (`rotated` = replaced an existing key).
/// Shared by `genesis` + `d2-gen` so both get the same atomic semantics.
fn store_generated_key(
    conn: &Connection,
    vnode: &str,
    mesh: &str,
    service: &str,
    username: &str,
    notes: &str,
    force: bool,
) -> Result<Option<(String, String, bool)>> {
    ensure_secrets_schema(conn)?;
    let sk = SigningKey::generate(&mut OsRng);
    let seed_b64 = B64.encode(sk.to_bytes());
    let pub_b64 = B64.encode(sk.verifying_key().to_bytes());
    if force {
        let rotated = upsert_seed(conn, vnode, mesh, service, username, &seed_b64, notes)?;
        Ok(Some((seed_b64, pub_b64, rotated)))
    } else {
        match insert_seed(conn, vnode, mesh, service, username, &seed_b64, notes) {
            Ok(()) => Ok(Some((seed_b64, pub_b64, false))),
            Err(e) if is_unique_violation(&e) => Ok(None),
            Err(e) => Err(anyhow::Error::new(e).context("store generated key")),
        }
    }
}

/// The genesis signing key — `(vnode="mesh", service="mesh-genesis", username="genesis")`.
fn fetch_seed(conn: &Connection, mesh: &str) -> Result<Option<String>> {
    fetch_seed_row(conn, SECRETS_VNODE, mesh, SECRETS_SERVICE, GENESIS_KEY_ID)
}

fn load_signing_key(db: &Path, mesh: &str) -> Result<SigningKey> {
    let conn = Connection::open(db).with_context(|| format!("open {}", db.display()))?;
    let seed_b64 = fetch_seed(&conn, mesh)?.ok_or_else(|| {
        anyhow!(
            "no genesis key for mesh {mesh:?} in {} — run `cosmix-mesh-sign genesis` first",
            db.display()
        )
    })?;
    let seed = B64
        .decode(seed_b64.trim())
        .context("genesis key is not valid base64")?;
    let seed: [u8; 32] = seed
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("genesis key is not a 32-byte Ed25519 seed"))?;
    Ok(SigningKey::from_bytes(&seed))
}

/// Create the `secrets` table + uniqueness index if they don't exist, so
/// `genesis` works against a brand-new / empty DB (not just the operator's
/// pre-populated one). Matches the existing secrets.db schema.
fn ensure_secrets_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS secrets (
            id INTEGER PRIMARY KEY,
            vnode TEXT, domain TEXT, service TEXT NOT NULL, username TEXT NOT NULL,
            password TEXT NOT NULL, notes TEXT,
            created_at TEXT DEFAULT (datetime('now','localtime')),
            updated_at TEXT DEFAULT (datetime('now','localtime'))
         );
         CREATE UNIQUE INDEX IF NOT EXISTS idx_unique_entry
            ON secrets(vnode, domain, service, username);",
    )
    .context("ensure secrets schema")
}

fn cmd_genesis(db: &Path, mesh: &str, force: bool) -> Result<()> {
    let conn = Connection::open(db).with_context(|| format!("open {}", db.display()))?;
    let notes = "SPEC 13 inventory trust root — Ed25519 genesis signing key (base64 32-byte seed)";
    let (_seed, pub_b64, rotated) = store_generated_key(
        &conn,
        SECRETS_VNODE,
        mesh,
        SECRETS_SERVICE,
        GENESIS_KEY_ID,
        notes,
        force,
    )?
    .ok_or_else(|| {
        anyhow!(
            "a genesis key already exists for mesh {mesh:?} in {} — refusing to overwrite.\n\
             Rotating genesis INVALIDATES every prior signature and is an out-of-band \
             re-provision (§7.4/§6.4); pass --force only if you mean it.",
            db.display()
        )
    })?;

    println!(
        "genesis key {} for mesh {mesh:?}",
        if rotated { "ROTATED" } else { "created" }
    );
    println!("  key_id : {GENESIS_KEY_ID}");
    println!("  pubkey : {pub_b64}  (Ed25519, base64)");
    println!(
        "  stored : {}  (vnode={SECRETS_VNODE}, domain={mesh}, service={SECRETS_SERVICE}, username={GENESIS_KEY_ID})",
        db.display()
    );
    println!(
        "\nProvision this PUBLIC key out-of-band to every node as the genesis verify key (§7.4)."
    );
    Ok(())
}

// ---------------------------------------------------------------------
// d2 admission keys (SPEC 13 §9a / slice 2-d)
// ---------------------------------------------------------------------

/// Derive the base64 Ed25519 PUBLIC key from a base64 32-byte seed.
fn pubkey_from_seed_b64(seed_b64: &str) -> Result<String> {
    let seed = B64
        .decode(seed_b64.trim())
        .context("d2 key is not valid base64")?;
    let seed: [u8; 32] = seed
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("d2 key is not a 32-byte Ed25519 seed"))?;
    Ok(B64.encode(SigningKey::from_bytes(&seed).verifying_key().to_bytes()))
}

/// Generate + store a node's d2 keypair against an open connection. Returns
/// `(seed_b64, pub_b64, rotated)` — `rotated` is true if it replaced an existing
/// key. Atomically refuses to overwrite unless `force`. The testable core of
/// [`cmd_d2gen`].
fn gen_d2(
    conn: &Connection,
    mesh: &str,
    node: &str,
    force: bool,
) -> Result<(String, String, bool)> {
    let notes =
        format!("SPEC 13 §9a d2 admission key for node {node} (base64 32-byte Ed25519 seed)");
    store_generated_key(conn, node, mesh, D2_SERVICE, D2_KEY_ID, &notes, force)?.ok_or_else(|| {
        anyhow!(
            "a d2 key already exists for node {node:?} (mesh {mesh:?}) — refusing to overwrite.\n\
             Rotating a node's d2 key invalidates its in-flight admission proofs; pass --force \
             only if you mean it (then re-author + re-sign + push the inventory)."
        )
    })
}

fn cmd_d2gen(db: &Path, mesh: &str, node: &str, force: bool) -> Result<()> {
    let conn = Connection::open(db).with_context(|| format!("open {}", db.display()))?;
    let (_seed, pub_b64, rotated) = gen_d2(&conn, mesh, node, force)?;
    println!(
        "d2 key {} for node {node:?} (mesh {mesh:?})",
        if rotated { "ROTATED" } else { "created" }
    );
    println!("  pubkey : {pub_b64}  (Ed25519, base64)");
    println!(
        "  stored : {}  (vnode={node}, domain={mesh}, service={D2_SERVICE}, username={D2_KEY_ID})",
        db.display()
    );
    println!(
        "\nAuthor this PUBLIC key into {node}'s credentials[] in inventory.mix, then re-sign + push:"
    );
    println!(
        "    {{ kind: \"d2\", pubkey: \"{pub_b64}\", from_epoch: <epoch>, until_epoch: nil }}"
    );
    Ok(())
}

fn cmd_d2pubkey(db: &Path, mesh: &str, node: &str) -> Result<()> {
    let conn = Connection::open(db).with_context(|| format!("open {}", db.display()))?;
    let seed_b64 = fetch_seed_row(&conn, node, mesh, D2_SERVICE, D2_KEY_ID)?.ok_or_else(|| {
        anyhow!(
            "no d2 key for node {node:?} (mesh {mesh:?}) in {} — run `cosmix-mesh-sign d2-gen {node}` first",
            db.display()
        )
    })?;
    // Bare base64, no trailing text — suitable to splice into inventory.mix.
    println!("{}", pubkey_from_seed_b64(&seed_b64)?);
    Ok(())
}

// ---------------------------------------------------------------------
// sign / verify
// ---------------------------------------------------------------------

fn cmd_sign(
    db: &Path,
    mesh: &str,
    inventory: &Path,
    out: &Path,
    valid_days: i64,
    recovery: Option<u64>,
    recovery_generation: Option<u64>,
) -> Result<()> {
    if recovery.is_some() {
        eprintln!(
            "WARNING: --recovery does not check fleet epoch history; choose the next normal epoch deliberately after recovery (recovery_generation is the security barrier)."
        );
    }
    let sk = load_signing_key(db, mesh)?;
    let pub_b64 = B64.encode(sk.verifying_key().to_bytes());

    // Read + validate the one authored inventory through the SAME parser and
    // integer-aware JSON conversion used by --against-authored.
    let mut value = load_authored_inventory_json(inventory)?;
    let obj = value
        .as_object_mut()
        .ok_or_else(|| anyhow!("inventory is not a map"))?;

    // Drop the Phase-1a `unsigned` provenance marker — the signed payload
    // has no such field.
    obj.remove("unsigned");

    // Add the §7.1 signing fields the authored unsigned inventory lacks.
    let now = Utc::now();
    let valid_until = now + Duration::days(valid_days);
    obj.insert("canonical_encoding".into(), json!(CANONICAL_ENCODING_V1));
    obj.insert(
        "signed_at".into(),
        json!(now.to_rfc3339_opts(SecondsFormat::Secs, true)),
    );
    obj.insert(
        "valid_until".into(),
        json!(valid_until.to_rfc3339_opts(SecondsFormat::Secs, true)),
    );
    obj.insert(
        "verify_keys".into(),
        json!([{
            "key_id": GENESIS_KEY_ID,
            "pubkey": pub_b64,
            "key_type": ALG_ED25519,
            "status": "active",
        }]),
    );

    // Recovery inventory (§6.4): genesis-signed `recovery: true` +
    // `recovery_generation` supersedes the epoch axis. (The verifier requires
    // the generation to strictly exceed the node's last; that's the operator's
    // responsibility to set, and a stale one is rejected.)
    if let Some(generation) = recovery {
        obj.insert("recovery".into(), json!(true));
        obj.insert("recovery_generation".into(), json!(generation));
    } else if let Some(generation) = recovery_generation {
        obj.insert("recovery_generation".into(), json!(generation));
    }

    // Into the typed payload — deny_unknown_fields here validates we built
    // exactly the §7.1 shape (a stray or missing field fails loudly).
    let payload: InventoryPayload = serde_json::from_value(value)
        .context("the inventory does not match the §7.1 payload shape")?;

    // The signed inventory is also the shared routing authority. Apply the
    // exact semantic gate noded and wgd use before creating a signature, so
    // an authenticated but unroutable epoch cannot be published.
    let routing_view = strict_routing_view(&payload.members, &payload.subnet)
        .map_err(|e| anyhow!("inventory routing view is unusable: {e}"))?;

    // Sign the canonical bytes (the one shared canonicaliser).
    let sig = sk.sign(&payload.canonical_bytes());
    let signed = SignedInventory {
        signatures: vec![InvSignature {
            key_id: GENESIS_KEY_ID.into(),
            alg: ALG_ED25519.into(),
            sig: B64.encode(sig.to_bytes()),
        }],
        payload,
    };

    let mut text = serde_json::to_string_pretty(&signed)?;
    text.push('\n');

    // Self-verify the EMITTED bytes through the exact path a node uses
    // (parse the JSON, then verify) BEFORE writing it — an artifact that
    // doesn't self-verify (e.g. `--recovery 0`, a stale generation) must
    // never land on disk. This is a cryptographic + structural self-check,
    // not a node-staleness check (the synthetic baseline is epoch 0 /
    // generation 0, so a real node with a higher cached epoch could still
    // reject as stale; that is the node's job).
    let state = trust_state(sk.verifying_key().to_bytes().to_vec());
    let reparsed = SignedInventory::parse(text.as_bytes())
        .map_err(|e| anyhow!("re-parsing the emitted artifact FAILED: {e}"))?;
    let accepted = reparsed
        .verify(&state)
        .map_err(|e| anyhow!("self-verify of the emitted artifact FAILED: {e}"))?;

    // Only now that it self-verifies do we write it — and atomically, because
    // this artifact is the mesh's trust ROOT. `std::fs::write` truncates the
    // destination in place: a crash between truncate and the last byte leaves a
    // half-written `inventory.signed` where a valid one used to be. That failure
    // is loud rather than silent (a truncated payload fails signature
    // verification), but "the trust root is now unparseable" is still an outage,
    // and it destroys the artifact that was previously good. It also FOLLOWS a
    // destination symlink, so a link planted at `out` writes the signed payload
    // wherever it points.
    write_atomic(out, text.as_bytes()).with_context(|| format!("write {}", out.display()))?;

    println!("signed {} -> {}", inventory.display(), out.display());
    println!(
        "  epoch={}  signatures={}  routing_members={}  recovery_generation={}  payload_recovery_generation={}  via_recovery={}  verified_by={:?}  (self-verify OK)",
        accepted.epoch,
        signed.signatures.len(),
        routing_view.len(),
        accepted.recovery_generation,
        signed
            .payload
            .recovery_generation
            .map_or_else(|| "null".to_string(), |generation| generation.to_string()),
        accepted.via_recovery,
        accepted.verified_by
    );
    if recovery.is_none() && recovery_generation.is_none() {
        eprintln!(
            "WARNING: signed a legacy generation-silent normal payload; any node whose cached recovery generation exceeds 0 will refuse it with NormalMissingRecoveryGeneration. Pass --recovery-generation <floor> for post-recovery fleets."
        );
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct VerifyOptions<'a> {
    genesis_pub: Option<&'a Path>,
    genesis_pub_base64: Option<&'a str>,
    expected_mesh: Option<&'a str>,
    include_payload: bool,
    against_authored: Option<&'a Path>,
}

fn emit_verify(
    db: &Path,
    mesh: &str,
    signed_path: &Path,
    json_output: bool,
    options: VerifyOptions<'_>,
) -> Result<()> {
    match cmd_verify(db, mesh, signed_path, options) {
        Ok(report) => {
            if json_output {
                println!("{}", serde_json::to_string(&report)?);
            } else {
                println!(
                    "VERIFIED {}\n  epoch={}  mesh={}  recovery_generation={}  payload_recovery_generation={}  via_recovery={}  verified_by={}",
                    signed_path.display(),
                    report["epoch"],
                    report["mesh"].as_str().unwrap_or(""),
                    report["recovery_generation"],
                    report["payload_recovery_generation"],
                    report["via_recovery"],
                    report["verified_by"]
                );
                if report["payload_recovery_generation"].is_null() {
                    eprintln!(
                        "NOTE: legacy generation-silent payload; this command verified it against a synthetic recovery-generation floor of 0. A node whose cached generation exceeds 0 refuses it with NormalMissingRecoveryGeneration."
                    );
                }
                if let Some(against) = report.get("against_authored") {
                    println!(
                        "  authored_match={}  authoring_blake3={}",
                        against["matches"],
                        against["signed_authoring_blake3"].as_str().unwrap_or("")
                    );
                }
            }
            Ok(())
        }
        Err(error) if json_output => {
            println!("{}", serde_json::to_string(&error.to_json())?);
            std::process::exit(1);
        }
        Err(error) => Err(error.into()),
    }
}

#[derive(Debug)]
struct VerifyCommandError {
    code: &'static str,
    message: String,
    details: Option<Json>,
}

impl VerifyCommandError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: None,
        }
    }

    fn with_details(mut self, details: Json) -> Self {
        self.details = Some(details);
        self
    }

    fn to_json(&self) -> Json {
        let mut report = json!({
            "ok": false,
            "format": VERIFY_JSON_FORMAT,
            "error": {
                "code": self.code,
                "message": self.message,
            },
        });
        if let Some(details) = &self.details {
            report["details"] = details.clone();
        }
        report
    }
}

impl std::fmt::Display for VerifyCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for VerifyCommandError {}

fn cmd_verify(
    db: &Path,
    mesh: &str,
    signed_path: &Path,
    options: VerifyOptions<'_>,
) -> std::result::Result<Json, VerifyCommandError> {
    let genesis_key =
        load_verification_key(db, mesh, options.genesis_pub, options.genesis_pub_base64)?;
    let state = trust_state(genesis_key);

    let wire = std::fs::read(signed_path).map_err(|e| {
        VerifyCommandError::new(
            "read_signed",
            format!("read {}: {e}", signed_path.display()),
        )
    })?;
    let signed = SignedInventory::parse(&wire).map_err(|e| {
        VerifyCommandError::new(
            "parse_signed",
            format!("parse {}: {e}", signed_path.display()),
        )
    })?;
    let accepted = signed.verify(&state).map_err(|e| {
        VerifyCommandError::new(
            "verification_failed",
            format!("verification FAILED for {}: {e}", signed_path.display()),
        )
    })?;
    let routing_view = strict_routing_view(&signed.payload.members, &signed.payload.subnet)
        .map_err(|e| {
            VerifyCommandError::new(
                "routing_view_invalid",
                format!(
                    "authenticated routing view FAILED for {}: {e}",
                    signed_path.display()
                ),
            )
        })?;

    if let Some(expected) = options.expected_mesh
        && signed.payload.mesh != expected
    {
        return Err(VerifyCommandError::new(
            "mesh_mismatch",
            format!(
                "authenticated payload mesh {:?} does not match expected mesh {expected:?}",
                signed.payload.mesh
            ),
        )
        .with_details(json!({
            "expected_mesh": expected,
            "actual_mesh": signed.payload.mesh,
        })));
    }

    let canonical_blake3 = signed.payload.canonical_blake3();
    let signed_authoring_blake3 = signed.payload.authoring_blake3();
    let against_result = if let Some(authored_path) = options.against_authored {
        let authored = load_authored_inventory_json(authored_path).map_err(|e| {
            VerifyCommandError::new(
                "parse_authored",
                format!("parse authored {}: {e:#}", authored_path.display()),
            )
        })?;
        let authored_authoring_blake3 = authoring_blake3_for_value(&authored);
        let matches = signed_authoring_blake3 == authored_authoring_blake3;
        let comparison = json!({
            "matches": matches,
            "signed_authoring_blake3": signed_authoring_blake3,
            "authored_authoring_blake3": authored_authoring_blake3,
        });
        if !matches {
            return Err(VerifyCommandError::new(
                "authoring_mismatch",
                format!(
                    "authored inventory {} does not match the authenticated payload: signed authoring_blake3={}, authored authoring_blake3={}",
                    authored_path.display(),
                    comparison["signed_authoring_blake3"].as_str().unwrap_or(""),
                    comparison["authored_authoring_blake3"].as_str().unwrap_or("")
                ),
            )
            .with_details(json!({ "against_authored": comparison })));
        }
        Some(comparison)
    } else {
        None
    };

    let member_count = signed
        .payload
        .members
        .as_array()
        .map_or(0, std::vec::Vec::len);
    let mut report = json!({
        "ok": true,
        "format": VERIFY_JSON_FORMAT,
        "epoch": accepted.epoch,
        "mesh": signed.payload.mesh,
        "schema_version": signed.payload.schema_version,
        "recovery_generation": accepted.recovery_generation,
        "payload_recovery_generation": signed.payload.recovery_generation,
        "via_recovery": accepted.via_recovery,
        "verified_by": accepted.verified_by,
        "canonical_blake3": canonical_blake3,
        "authoring_blake3": signed_authoring_blake3,
        "member_count": member_count,
        "routing_view": {
            "valid": true,
            "members": routing_view.iter().map(RoutingMember::to_json).collect::<Vec<_>>(),
        },
    });
    if let Some(against) = against_result {
        report["against_authored"] = against;
    }
    if options.include_payload {
        report["payload"] = json!(signed.payload);
    }
    Ok(report)
}

fn load_verification_key(
    db: &Path,
    mesh: &str,
    genesis_pub: Option<&Path>,
    genesis_pub_base64: Option<&str>,
) -> std::result::Result<Vec<u8>, VerifyCommandError> {
    let encoded = if let Some(path) = genesis_pub {
        std::fs::read_to_string(path).map_err(|e| {
            VerifyCommandError::new(
                "read_genesis_pub",
                format!("read genesis public key {}: {e}", path.display()),
            )
        })?
    } else if let Some(encoded) = genesis_pub_base64 {
        encoded.to_owned()
    } else {
        let signing_key = load_signing_key(db, mesh).map_err(|e| {
            VerifyCommandError::new(
                "load_signing_key",
                format!("load genesis signing key for mesh {mesh:?}: {e:#}"),
            )
        })?;
        return Ok(signing_key.verifying_key().to_bytes().to_vec());
    };

    let decoded = B64.decode(encoded.trim()).map_err(|e| {
        VerifyCommandError::new(
            "decode_genesis_pub",
            format!("genesis public key is not valid base64: {e}"),
        )
    })?;
    if decoded.len() != ED25519_PUBLIC_KEY_LEN {
        return Err(VerifyCommandError::new(
            "invalid_genesis_pub_length",
            format!(
                "genesis public key is {} bytes, expected {ED25519_PUBLIC_KEY_LEN}",
                decoded.len()
            ),
        ));
    }
    Ok(decoded)
}

/// A single-genesis-key node trust state (the testbed shape): trusts only
/// genesis, baseline epoch/recovery_generation 0.
/// Write `bytes` to `out` so that a reader ever sees the whole file or the
/// previous one — never a prefix of this one.
///
/// Stage into a sibling temp (same directory, therefore the same filesystem, so
/// the rename is a true rename and not a copy), fsync the contents, rename over
/// the destination, then fsync the *directory* so the rename itself is durable.
/// Without that last fsync the data is on disk but the name may not be, and a
/// power cut can restore the old name pointing at the new inode's predecessor.
///
/// `create_new(true)` on the temp is the anti-symlink measure: it fails with
/// EEXIST on anything already at that path, including a symlink, so this never
/// writes through a planted link. The destination is not opened at all — rename
/// replaces a symlink AT `out` rather than following it.
fn write_atomic(out: &Path, bytes: &[u8]) -> Result<()> {
    // `Path::parent()` on a bare filename returns Some("") — NOT None — so the
    // obvious `unwrap_or(".")` never fires and the directory fsync below then
    // opens "" and fails with ENOENT. That failure arrives AFTER the rename,
    // i.e. the command reports failure having already replaced the file. Filter
    // the empty parent explicitly. (Reproduced: `cosmix-mesh-sign sign
    // inventory.mix -o inventory.signed` from the inventory's own directory.)
    let dir = out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let stem = out
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "out".into());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = dir.join(format!(".{}.tmp.{}.{}", stem, std::process::id(), nanos));

    // Create FIRST, and only arm the cleanup once the create has succeeded.
    // An unconditional cleanup deletes the very thing `create_new` refused to
    // write through: if the temp name already exists (a planted symlink, a
    // collision), the error path would remove somebody else's path, turning a
    // guard that promises "this never writes through a link" into one that
    // silently unlinks it.
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        // A floor, not the final word: open(2) masks this with the process
        // umask, so under `umask 077` this alone yields 0600 and publishes a
        // cache the unprivileged noded cannot read. The fchmod below is what
        // actually sets the mode. Keeping the flag as well means the file is
        // never briefly MORE permissive than intended between the two calls.
        .mode(0o644)
        .open(&tmp)
        .with_context(|| format!("stage {}", tmp.display()))?;

    // Set the mode explicitly, because the open(2) mode above is umask-masked
    // and this artifact is world-readable by design (every node reads it).
    // fchmod(2) takes no umask. Done on the fd, before the rename, so the
    // destination is never observable in the wrong mode — and because the
    // rename replaces the destination's inode, the destination's old mode does
    // not carry over the way an in-place `fs::write` would have preserved it.
    if let Err(e) = f.set_permissions(std::fs::Permissions::from_mode(0o644)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(anyhow::Error::new(e).context(format!("chmod {}", tmp.display())));
    }

    let staged = |e: anyhow::Error| -> anyhow::Error {
        // Best-effort: we created this exact path and nothing else has been
        // told its name, so removing it is ours to do. If the remove fails the
        // original error is still the one worth reporting.
        let _ = std::fs::remove_file(&tmp);
        e
    };

    if let Err(e) = f.write_all(bytes) {
        return Err(staged(
            anyhow::Error::new(e).context(format!("write {}", tmp.display())),
        ));
    }
    if let Err(e) = f.sync_all() {
        return Err(staged(
            anyhow::Error::new(e).context(format!("fsync {}", tmp.display())),
        ));
    }
    drop(f);
    if let Err(e) = std::fs::rename(&tmp, out) {
        return Err(staged(anyhow::Error::new(e).context(format!(
            "rename {} -> {}",
            tmp.display(),
            out.display()
        ))));
    }
    // Past the rename the temp name no longer exists and the destination is
    // already the new file, so there is nothing left to clean up — a failure
    // here is a durability failure, not a "did it land" failure, and deleting
    // anything now would destroy the artifact we just placed.
    std::fs::File::open(dir)
        .and_then(|d| d.sync_all())
        .with_context(|| format!("fsync dir {}", dir.display()))?;
    Ok(())
}

fn trust_state(genesis_pubkey: Vec<u8>) -> NodeTrustState {
    NodeTrustState {
        genesis_key_id: GENESIS_KEY_ID.into(),
        trusted_keys: vec![TrustedKey {
            key_id: GENESIS_KEY_ID.into(),
            pubkey: genesis_pubkey,
            status: KeyStatus::Active,
        }],
        last_epoch: 0,
        last_recovery_generation: 0,
        last_canonical_hash: None,
    }
}

// ---------------------------------------------------------------------
// Mix Value → JSON (integer-aware)
// ---------------------------------------------------------------------

/// Reject an authored inventory that isn't the Phase-1a *unsigned* form
/// (§7.7) or that already carries a signer-owned field. This is the
/// fail-closed guard that lets the rest of `cmd_sign` safely insert the
/// signer's own fields before the typed `deny_unknown_fields` validation.
fn check_authored_unsigned(obj: &serde_json::Map<String, Json>) -> Result<()> {
    if obj.get("unsigned") != Some(&Json::Bool(true)) {
        bail!(
            "authored inventory must carry `unsigned: true` (the §7.7 Phase-1a \
             marker) — refusing to sign something not marked as an unsigned input"
        );
    }
    for &owned in SIGNER_OWNED_FIELDS {
        if obj.contains_key(owned) {
            bail!(
                "authored inventory unexpectedly contains the signer-owned field \
                 `{owned}` — refusing to sign (is this an already-signed payload?)"
            );
        }
    }
    Ok(())
}

/// Parse an authored strict-data inventory through the one Mix parser and
/// integer-aware conversion shared by signing and `verify --against-authored`.
/// The returned JSON deliberately retains `unsigned:true`;
/// [`authoring_blake3_for_value`] owns its removal for hashing, while `cmd_sign`
/// removes it before constructing the signed payload.
fn load_authored_inventory_json(path: &Path) -> Result<Json> {
    let mix =
        cosmix_config::load_mix_data(path).map_err(|e| anyhow!("parse {}: {e}", path.display()))?;
    let inner = mix_get(&mix, "inventory").ok_or_else(|| {
        anyhow!(
            "{} has no top-level `inventory:` key (is it the strict-data form?)",
            path.display()
        )
    })?;
    let value = mix_to_json(inner)?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("inventory is not a map"))?;
    check_authored_unsigned(object)?;
    Ok(value)
}

fn mix_get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    match v {
        Value::Map(m) => m.get(key),
        _ => None,
    }
}

/// The largest integer an f64 represents exactly (2^53 − 1). Mix lexes
/// every number as f64, so an authored integer beyond this is already
/// rounded by the time we see it — a trust-root signer MUST refuse to
/// sign a value that does not equal what the operator wrote.
const MAX_SAFE_INT: f64 = MAX_EXACT_MIX_INTEGER as f64;

/// Faithfully convert a Mix `Value` to a `serde_json::Value`, failing
/// CLOSED on anything that can't be represented exactly. Whole numbers
/// are emitted as integers so epoch/credential fields read as `1`, not
/// `1.0`. The result only has to be *correct* (the signer
/// re-canonicalises in Rust and emits JSON the verifier re-parses), but
/// "correct" for a trust root means byte-for-byte what was authored.
fn mix_to_json(v: &Value) -> Result<Json> {
    Ok(match v {
        Value::Nil => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::String(s) => Json::String(s.clone()),
        Value::Number(n) => num_to_json(*n)?,
        Value::List(l) => Json::Array(l.iter().map(mix_to_json).collect::<Result<_>>()?),
        Value::Map(m) => {
            let mut obj = serde_json::Map::with_capacity(m.len());
            for (k, v) in m.iter() {
                obj.insert(k.clone(), mix_to_json(v)?);
            }
            Json::Object(obj)
        }
        // Strict-data (load_data/parse_data) can produce none of these —
        // no bytes literal, no function value. Fail closed rather than
        // silently reshaping the signed tree if that parser invariant
        // ever changes.
        Value::Bytes(_) => bail!("strict-data inventory unexpectedly contains a bytes value"),
        Value::Function(_) => bail!("strict-data inventory unexpectedly contains a function value"),
        Value::Buffer(_) => bail!("strict-data inventory unexpectedly contains a buffer value"),
    })
}

fn num_to_json(n: f64) -> Result<Json> {
    if !n.is_finite() {
        bail!("inventory contains a non-finite number ({n}), which cannot be signed");
    }
    if n.fract() == 0.0 {
        if n.abs() > MAX_SAFE_INT {
            bail!(
                "inventory integer {n:.0} exceeds the f64 lossless range (±2^53−1); \
                 Mix lexes numbers as f64, so it cannot be faithfully signed"
            );
        }
        return Ok(if n >= 0.0 {
            json!(n as u64)
        } else {
            json!(n as i64)
        });
    }
    // Fractional value — round-trips as f64. (A typed payload field like
    // epoch would then fail `from_value::<u64>`, which is correct.)
    serde_json::Number::from_f64(n)
        .map(Json::Number)
        .ok_or_else(|| anyhow!("number {n} cannot be represented in JSON"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- write_atomic (the trust root's write path) -----------------

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "cosmix-mesh-sign-test-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn signing_fixture(name: &str, mesh_ip: &str) -> (PathBuf, PathBuf, PathBuf) {
        let dir = scratch(name);
        let db = dir.join("secrets.db");
        let inventory = dir.join("inventory.mix");
        let out = dir.join("inventory.signed");
        let conn = Connection::open(&db).unwrap();
        ensure_secrets_schema(&conn).unwrap();
        insert_seed(
            &conn,
            SECRETS_VNODE,
            "example.internal",
            SECRETS_SERVICE,
            GENESIS_KEY_ID,
            &B64.encode([21u8; 32]),
            "routing gate test key",
        )
        .unwrap();
        drop(conn);
        std::fs::write(
            &inventory,
            format!(
                concat!(
                    "inventory: {{\n",
                    "  schema_version: 1,\n",
                    "  mesh: \"example.internal\",\n",
                    "  subnet: \"192.0.2.0/24\",\n",
                    "  epoch: 7,\n",
                    "  hub: [\"alpha\"],\n",
                    "  members: [{{\n",
                    "    name: \"alpha\", mesh_ip: \"{}\", bus: true, status: \"active\",\n",
                    "    credentials: [], last_touched_epoch: 7\n",
                    "  }}],\n",
                    "  unsigned: true\n",
                    "}}\n"
                ),
                mesh_ip
            ),
        )
        .unwrap();
        (db, inventory, out)
    }

    #[test]
    fn sign_refuses_out_of_subnet_active_member_before_writing() {
        let (db, inventory, out) = signing_fixture("routing-reject", "198.51.100.5");

        let error =
            cmd_sign(&db, "example.internal", &inventory, &out, 90, None, None).unwrap_err();
        let message = format!("{error:#}");

        assert!(
            message.contains("inventory routing view is unusable"),
            "{message}"
        );
        assert!(message.contains("member[0]"), "{message}");
        assert!(message.contains("outside inventory subnet"), "{message}");
        assert!(!out.exists(), "invalid inventory was signed and published");
    }

    #[test]
    fn sign_refuses_invalid_bus_label_before_writing() {
        let (db, inventory, out) = signing_fixture("label-reject", "192.0.2.5");
        let source = std::fs::read_to_string(&inventory)
            .unwrap()
            .replace("name: \"alpha\"", "name: \"Beta\"");
        std::fs::write(&inventory, source).unwrap();

        let error =
            cmd_sign(&db, "example.internal", &inventory, &out, 90, None, None).unwrap_err();
        let message = format!("{error:#}");

        assert!(
            message.contains("inventory routing view is unusable"),
            "{message}"
        );
        assert!(message.contains("invalid bus label \"Beta\""), "{message}");
        assert!(message.contains("SPEC 01 §4.1"), "{message}");
        assert!(!out.exists(), "invalid inventory was signed and published");
    }

    #[test]
    fn sign_refuses_invalid_active_bus_port_before_writing() {
        let (db, inventory, out) = signing_fixture("port-reject", "192.0.2.5");
        let source = std::fs::read_to_string(&inventory)
            .unwrap()
            .replace("bus: true", "bus: true, noded_port: 0");
        std::fs::write(&inventory, source).unwrap();

        let error =
            cmd_sign(&db, "example.internal", &inventory, &out, 90, None, None).unwrap_err();
        let message = format!("{error:#}");

        assert!(
            message.contains("inventory routing view is unusable"),
            "{message}"
        );
        assert!(message.contains("noded_port"), "{message}");
        assert!(message.contains("1..=65535"), "{message}");
        assert!(!out.exists(), "invalid inventory was signed and published");
    }

    #[test]
    fn sign_accepts_valid_shared_routing_view() {
        let (db, inventory, out) = signing_fixture("routing-accept", "192.0.2.5");
        let source = std::fs::read_to_string(&inventory)
            .unwrap()
            .replace("bus: true", "bus: true, noded_port: 4300");
        std::fs::write(&inventory, source).unwrap();

        cmd_sign(&db, "example.internal", &inventory, &out, 90, None, None).unwrap();

        let signed = SignedInventory::parse(&std::fs::read(&out).unwrap()).unwrap();
        let view = strict_routing_view(&signed.payload.members, &signed.payload.subnet).unwrap();
        assert_eq!(view.len(), 1);
        assert!(matches!(
            view[0],
            RoutingMember::ActiveBus {
                noded_port: 4300,
                ..
            }
        ));
    }

    #[test]
    fn sign_stamps_normal_recovery_generation_without_recovery_marker() {
        let (db, inventory, out) = signing_fixture("normal-generation", "192.0.2.5");

        cmd_sign(&db, "example.internal", &inventory, &out, 90, None, Some(0)).unwrap();

        let signed = SignedInventory::parse(&std::fs::read(&out).unwrap()).unwrap();
        assert_eq!(signed.payload.recovery, None);
        assert_eq!(signed.payload.recovery_generation, Some(0));
    }

    #[test]
    fn recovery_flags_are_mutually_exclusive() {
        let error = Cli::try_parse_from([
            "cosmix-mesh-sign",
            "sign",
            "inventory.mix",
            "--out",
            "inventory.signed",
            "--recovery",
            "1",
            "--recovery-generation",
            "1",
        ])
        .err()
        .expect("the two recovery modes must conflict");
        let message = error.to_string();
        assert!(message.contains("cannot be used with"), "{message}");
        assert!(message.contains("--recovery"), "{message}");
        assert!(message.contains("--recovery-generation"), "{message}");
    }

    #[test]
    fn recovery_flags_refuse_values_mix_cannot_compare_exactly() {
        assert_eq!(
            parse_recovery_generation("9007199254740991"),
            Ok(MAX_EXACT_MIX_INTEGER)
        );
        let error = parse_recovery_generation("9007199254740992").unwrap_err();
        assert!(error.contains("largest integer Mix can compare exactly"));
        assert!(error.contains("Mix numbers use f64"));

        for flag in ["--recovery", "--recovery-generation"] {
            let error = Cli::try_parse_from([
                "cosmix-mesh-sign",
                "sign",
                "inventory.mix",
                "--out",
                "inventory.signed",
                flag,
                "9007199254740992",
            ])
            .err()
            .expect("the CLI must apply the exact-integer ceiling to both flags");
            let message = error.to_string();
            assert!(message.contains("largest integer Mix can compare exactly"));
        }
    }

    /// A RELATIVE, single-component destination must succeed. `Path::parent()`
    /// answers Some("") there rather than None, and the first cut of
    /// write_atomic fsynced that empty path — reporting failure AFTER the
    /// rename had already replaced the file.
    #[test]
    fn write_atomic_accepts_a_bare_relative_destination() {
        let d = scratch("bare");
        let prev = std::env::current_dir().unwrap();
        // set_current_dir is process-global; this test is the only one that
        // touches it and restores it before asserting.
        std::env::set_current_dir(&d).unwrap();
        let r = write_atomic(Path::new("inventory.signed"), b"payload\n");
        std::env::set_current_dir(prev).unwrap();
        r.expect("a bare relative destination must not fail");
        assert_eq!(
            std::fs::read(d.join("inventory.signed")).unwrap(),
            b"payload\n"
        );
        std::fs::remove_dir_all(&d).ok();
    }

    /// A symlink AT the destination is REPLACED, not followed. The pre-fix
    /// `std::fs::write` wrote the signed payload through the link into whatever
    /// it pointed at, leaving the trust root's path still a symlink.
    #[test]
    fn write_atomic_replaces_a_destination_symlink_instead_of_following_it() {
        let d = scratch("symlink");
        let victim = d.join("victim");
        std::fs::write(&victim, b"VICTIM").unwrap();
        let dest = d.join("inventory.signed");
        std::os::unix::fs::symlink(&victim, &dest).unwrap();

        write_atomic(&dest, b"payload\n").unwrap();

        assert_eq!(
            std::fs::read(&victim).unwrap(),
            b"VICTIM",
            "victim written through"
        );
        assert!(
            !std::fs::symlink_metadata(&dest).unwrap().is_symlink(),
            "the destination is still a symlink"
        );
        assert_eq!(std::fs::read(&dest).unwrap(), b"payload\n");
        std::fs::remove_dir_all(&d).ok();
    }

    /// The published mode is 0644 REGARDLESS of the process umask, and the
    /// rename replaces the destination's inode, so the destination's old mode
    /// does not carry over the way an in-place write would have preserved it.
    ///
    /// The umask arm is the load-bearing one. Under the suite's usual 022 this
    /// test passed on `OpenOptions::mode(0o644)` alone — but open(2) masks that
    /// argument, so `umask 077` published 0600 and an unprivileged noded could
    /// not read the cache. A test that only ever runs at 022 cannot tell the
    /// two implementations apart; that is why it sets the umask itself.
    ///
    /// umask(2) is per-process, so the window between set and restore leaks to
    /// tests running in parallel. That is tolerable here only because this is
    /// the sole test in the crate that asserts a mode — the others check
    /// content and existence, which a stricter umask does not change.
    #[test]
    fn write_atomic_writes_an_explicit_mode() {
        use std::os::unix::fs::PermissionsExt as _;
        let d = scratch("mode");

        for umask in [0o022, 0o077] {
            let dest = d.join(format!("inventory.signed.{umask:o}"));
            std::fs::write(&dest, b"old").unwrap();
            std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o600)).unwrap();

            // SAFETY: umask(2) cannot fail and has no preconditions; it returns
            // the previous mask, which is restored immediately after the call
            // under test.
            let prev = unsafe { libc::umask(umask) };
            let r = write_atomic(&dest, b"new\n");
            unsafe { libc::umask(prev) };
            r.unwrap();

            assert_eq!(
                std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777,
                0o644,
                "published mode under umask {umask:o}"
            );
        }
        std::fs::remove_dir_all(&d).ok();
    }

    /// Nothing is left behind on the happy path — a leftover temp beside the
    /// trust root reads as a half-finished ceremony.
    #[test]
    fn write_atomic_leaves_no_temp_behind() {
        let d = scratch("debris");
        write_atomic(&d.join("inventory.signed"), b"x\n").unwrap();
        let names: Vec<String> = std::fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["inventory.signed".to_string()],
            "debris: {names:?}"
        );
        std::fs::remove_dir_all(&d).ok();
    }

    // ---- d2 admission keys (slice 2-d) -----------------------------

    /// A node's stored d2 PRIVATE seed signs an admission transcript that the
    /// broker's `admit()` accepts against the PUBLIC key d2-gen printed — the
    /// whole point of provisioning. Uses an in-memory secrets DB.
    #[test]
    fn d2_gen_produces_an_admittable_keypair() {
        use cosmix_mesh_trust::admission::{AdmissionTranscript, admit, sign_admission_transcript};

        let conn = Connection::open_in_memory().unwrap();
        let (seed_b64, pub_b64, rotated) = gen_d2(&conn, "bus", "delta", false).unwrap();
        assert!(!rotated, "first generation is a create, not a rotation");

        // The stored seed round-trips and matches the printed pubkey.
        let stored = fetch_seed_row(&conn, "delta", "bus", D2_SERVICE, D2_KEY_ID)
            .unwrap()
            .expect("d2 seed should be stored");
        assert_eq!(stored, seed_b64);
        assert_eq!(pubkey_from_seed_b64(&stored).unwrap(), pub_b64);

        // The node signs a transcript with its seed; the broker admits it
        // against the pubkey authored into the (opaque) member record.
        let seed = B64.decode(&seed_b64).unwrap();
        let t = AdmissionTranscript {
            mesh_fqdn: "bus".into(),
            claimed_source_node: "delta".into(),
            verifying_broker_node: "beta".into(),
            inventory_epoch: 1,
            session_id: vec![1, 2, 3],
            server_nonce: vec![4u8; 32],
            client_ephemeral: vec![0u8; 32],
            channel_binding_hash: vec![0u8; 32],
        };
        let sig = sign_admission_transcript(&seed, &t).unwrap();
        let member = json!({
            "name": "delta", "status": "active", "bus": true,
            "credentials": [
                { "kind": "d2", "pubkey": pub_b64, "from_epoch": 1, "until_epoch": null },
            ],
        });
        assert_eq!(admit(&member, &t, &sig, 1), Ok(()));
    }

    #[test]
    fn d2_gen_refuses_overwrite_without_force_and_rotates_with_it() {
        let conn = Connection::open_in_memory().unwrap();
        let (first_seed, _first_pub, _) = gen_d2(&conn, "bus", "delta", false).unwrap();
        // Second generation without --force is refused (no silent key loss).
        assert!(gen_d2(&conn, "bus", "delta", false).is_err());
        // The original key is untouched after the refusal.
        let still = fetch_seed_row(&conn, "delta", "bus", D2_SERVICE, D2_KEY_ID)
            .unwrap()
            .unwrap();
        assert_eq!(still, first_seed);
        // With --force it ROTATES to a new key.
        let (rotated_seed, _, rotated) = gen_d2(&conn, "bus", "delta", true).unwrap();
        assert!(rotated, "forced regeneration is a rotation");
        assert_ne!(rotated_seed, first_seed);
    }

    #[test]
    fn d2_keys_are_per_node_distinct() {
        // Two nodes get independent keys filed under their own vnode.
        let conn = Connection::open_in_memory().unwrap();
        let (_, delta_pub, _) = gen_d2(&conn, "bus", "delta", false).unwrap();
        let (_, gamma_pub, _) = gen_d2(&conn, "bus", "gamma", false).unwrap();
        assert_ne!(delta_pub, gamma_pub);
        assert_eq!(
            cmd_d2pubkey_value(&conn, "bus", "delta").unwrap(),
            Some(delta_pub)
        );
    }

    /// Test helper mirroring `cmd_d2pubkey`'s lookup without the stdout/IO.
    fn cmd_d2pubkey_value(conn: &Connection, mesh: &str, node: &str) -> Result<Option<String>> {
        fetch_seed_row(conn, node, mesh, D2_SERVICE, D2_KEY_ID)?
            .map(|s| pubkey_from_seed_b64(&s))
            .transpose()
    }

    #[test]
    fn num_to_json_keeps_whole_numbers_integer() {
        assert_eq!(num_to_json(1.0).unwrap(), json!(1));
        assert_eq!(num_to_json(0.0).unwrap(), json!(0));
        assert_eq!(num_to_json(255.0).unwrap(), json!(255));
        // fractional stays a float
        assert_eq!(num_to_json(2.5).unwrap(), json!(2.5));
    }

    #[test]
    fn num_to_json_fails_closed_on_unrepresentable_values() {
        // 2^53 − 1 is the last exactly-representable integer — fine.
        assert!(num_to_json(9_007_199_254_740_991.0).is_ok());
        // Beyond it, Mix's f64 lexing may already have rounded the
        // operator's integer — a trust-root signer must refuse it.
        assert!(num_to_json(9_007_199_254_740_993.0).is_err());
        // Non-finite values cannot be signed.
        assert!(num_to_json(f64::INFINITY).is_err());
        assert!(num_to_json(f64::NAN).is_err());
    }

    #[test]
    fn check_authored_unsigned_guards_the_input() {
        // The happy case: marked unsigned, no signer-owned fields.
        let ok: serde_json::Map<String, Json> = json!({ "unsigned": true, "epoch": 1 })
            .as_object()
            .unwrap()
            .clone();
        assert!(check_authored_unsigned(&ok).is_ok());

        // Missing / non-true unsigned marker → reject.
        for bad in [
            json!({ "epoch": 1 }),
            json!({ "unsigned": false }),
            json!({ "unsigned": "yes" }),
        ] {
            assert!(check_authored_unsigned(bad.as_object().unwrap()).is_err());
        }

        // Any pre-existing signer-owned field → reject.
        for &owned in SIGNER_OWNED_FIELDS {
            let mut bad = serde_json::Map::new();
            bad.insert("unsigned".into(), Json::Bool(true));
            bad.insert(owned.into(), json!("x"));
            assert!(
                check_authored_unsigned(&bad).is_err(),
                "field {owned} should be rejected"
            );
        }
    }

    #[test]
    fn mix_to_json_round_trips_a_member_record() {
        use cosmix_config::parse_mix_data;
        let v = parse_mix_data(
            "inv: { epoch: 1, members: [ { name: \"alpha\", bus: true, \
             credentials: [ { from_epoch: 1, until_epoch: nil } ] } ] }",
        )
        .unwrap();
        let inner = mix_get(&v, "inv").unwrap();
        let j = mix_to_json(inner).unwrap();
        assert_eq!(j["epoch"], json!(1)); // integer, not 1.0
        assert_eq!(j["members"][0]["name"], json!("alpha"));
        assert_eq!(j["members"][0]["bus"], json!(true));
        assert_eq!(j["members"][0]["credentials"][0]["from_epoch"], json!(1));
        assert_eq!(
            j["members"][0]["credentials"][0]["until_epoch"],
            json!(null)
        );
    }
}
