//! SPEC 13 1b-c — the authority plane: load + verify the signed inventory.
//!
//! On start, noded reads its locally-cached signed inventory
//! (`/var/lib/cosmix/noded/inventory.signed`, the plane-A cache, INV-5),
//! verifies it against the out-of-band-provisioned genesis verify key
//! (`/etc/cosmix/noded/genesis.pub`, §7.4), enforces epoch monotonicity
//! against the persisted `(epoch, recovery_generation, canonical hash)` baseline
//! (`/var/lib/cosmix/noded/inventory.baseline`, wipe-surviving, §6.4), and
//! exposes the accepted member set + posture (the `noded.inventory` verb).
//!
//! **Fail closed (§7.7) — but NON-FATAL.** A missing / unsigned / malformed
//! / failing / stale inventory leaves the node in the [`Posture::Unverified`]
//! posture — it does NOT treat that inventory as a trust root. Critically,
//! "fail closed" here means "do not grant authority," NOT "refuse to run":
//! `load_and_verify` never panics, exits, or blocks indefinitely, and `run()` does not
//! gate broker startup on the posture. An `Unverified` node boots and serves
//! exactly like a pre-1b node — the broker, routing, and every existing verb
//! are unaffected; only authority-consuming gates report/use `unverified`. This is what
//! makes a staged fleet roll safe: a bad inventory push on one node cannot
//! take that node down, it just doesn't advance to a verified trust root.
//! D2 admission consumes the verified full-member records. D1.4 routing also
//! consumes the strict typed routing view: a Verified posture routes only to
//! signed ActiveBus members and their signed broker ports, while a
//! never-Verified boot retains the derived `/etc` roster as a compatibility
//! fallback.
//!
//! **Reload (2-c-0b).** [`load_and_verify`] is the single verify path: it runs
//! at startup AND from the `inventory.signed` watcher (`noded.rs`
//! `spawn_inventory_reload_watcher`), which hot-swaps the posture behind an
//! `ArcSwap` on change. The reload never downgrades a `Verified` posture to
//! `Unverified` on a bad/partial push (keep last-known-good, §7.7). In
//! `off`/`observe` the reload grandfathers every session. The deployed
//! `enforce` implementation already performs membership-recheck revocation
//! teardown (`noded.rs` 2-c-2c), despite older §14/control planning text that
//! still describes that activation as blocked. D1.4 does not add or widen any
//! teardown behaviour.
//!
//! Single-genesis-key testbed simplification: the trusted set is re-derived
//! from `genesis.pub` on every boot. In-band-adopted `verify_keys`
//! (multi-key rotation, §6.4) are reported but not yet *persisted* as the
//! next boot's trust anchor — that lands with the rotation slice.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use cosmix_mesh_trust::inventory::{
    AcceptedInventory, KeyStatus, NodeTrustState, SignedInventory, TrustedKey,
};
pub use cosmix_mesh_trust::routing::RoutingMember;
use cosmix_mesh_trust::routing::strict_routing_view;

const GENESIS_KEY_ID: &str = "genesis";
const ED25519_PUBKEY_LEN: usize = 32;
static BASELINE_TMP_NONCE: AtomicU64 = AtomicU64::new(0);
const BASELINE_LOCK_RETRY: Duration = Duration::from_millis(75);
const BASELINE_LOCK_TIMEOUT: Duration = Duration::from_secs(2);

/// Where the authority plane reads/writes its files.
pub struct AuthorityPaths {
    /// The provisioned genesis verify key (base64 Ed25519 pubkey, §7.4).
    pub genesis_pub: PathBuf,
    /// The locally-cached signed inventory (plane-A cache, INV-5).
    pub signed: PathBuf,
    /// The persisted anti-rollback baseline (wipe-surviving, §6.4).
    pub baseline: PathBuf,
}

impl Default for AuthorityPaths {
    fn default() -> Self {
        Self {
            genesis_pub: PathBuf::from("/etc/cosmix/noded/genesis.pub"),
            signed: PathBuf::from("/var/lib/cosmix/noded/inventory.signed"),
            baseline: PathBuf::from("/var/lib/cosmix/noded/inventory.baseline"),
        }
    }
}

/// The node's authority posture after a load attempt.
#[derive(Debug, Clone)]
pub enum Posture {
    /// The signed inventory verified and is the accepted trust root.
    Verified(Accepted),
    /// Fail-closed: no trust root in force. Carries the cause for audit.
    Unverified { reason: String },
}

/// What was accepted (the §17 authority-plane identity + member set).
#[derive(Debug, Clone)]
pub struct Accepted {
    pub epoch: u64,
    pub recovery_generation: u64,
    pub via_recovery: bool,
    pub mesh: String,
    /// blake3 of the canonical payload bytes — the §17 inventory identity,
    /// so two nodes at the same epoch with different content is detectable.
    pub hash: String,
    /// Trusted key_ids whose signatures authorised this inventory.
    pub verified_by: Vec<String>,
    /// The adopted verify-key set (the new trust anchor set), with each
    /// key's `status` so a root-key roll is observable (§17).
    pub verify_keys: Vec<VerifyKeySummary>,
    pub members: Vec<MemberSummary>,
    /// Strict, typed view of the accepted member records. D1.4 routing derives
    /// its active remote table from the ActiveBus entries.
    pub routing_view: Vec<RoutingMember>,
    /// The full verified member records keyed by name — the credential-bearing
    /// records `admit()` (§9a, 2-c-1b) needs to select a node's `d2` pubkey.
    /// Kept INTERNAL to the admission gate (not surfaced by `to_json`, no
    /// credential leak); the summarised `members` is the public §17 view.
    pub members_full: std::collections::BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone)]
pub struct VerifyKeySummary {
    pub key_id: String,
    pub status: String,
}

#[derive(Debug, Clone)]
pub struct MemberSummary {
    pub name: String,
    pub mesh_ip: String,
    pub bus: bool,
    pub status: String,
}

impl Posture {
    /// JSON view for the `noded.inventory` verb / §17 observability. Admission
    /// consumes `members_full`; routing and `noded.peers` consume the strict
    /// typed routing view from the same atomically-published runtime snapshot.
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Posture::Verified(a) => serde_json::json!({
                "posture": "verified",
                "epoch": a.epoch,
                "recovery_generation": a.recovery_generation,
                "via_recovery": a.via_recovery,
                "mesh": a.mesh,
                "hash": a.hash,
                "verified_by": a.verified_by,
                "verify_keys": a.verify_keys.iter().map(|k| serde_json::json!({
                    "key_id": k.key_id,
                    "status": k.status,
                })).collect::<Vec<_>>(),
                "members": a.members.iter().map(|m| serde_json::json!({
                    "name": m.name,
                    "mesh_ip": m.mesh_ip,
                    "bus": m.bus,
                    "status": m.status,
                })).collect::<Vec<_>>(),
            }),
            Posture::Unverified { reason } => serde_json::json!({
                "posture": "unverified",
                "reason": reason,
            }),
        }
    }
}

/// Load + verify the cached signed inventory against the provisioned
/// genesis key, advancing + persisting the baseline on success. Never
/// panics: every failure path returns [`Posture::Unverified`] (fail closed).
pub fn load_and_verify(paths: &AuthorityPaths) -> Posture {
    let genesis_pub = match read_genesis_pub(&paths.genesis_pub) {
        Ok(k) => k,
        Err(reason) => return Posture::Unverified { reason },
    };

    let wire = match std::fs::read(&paths.signed) {
        Ok(b) => b,
        Err(e) => {
            return Posture::Unverified {
                reason: format!(
                    "no signed inventory cache at {}: {e}",
                    paths.signed.display()
                ),
            };
        }
    };
    let signed = match SignedInventory::parse(&wire) {
        Ok(s) => s,
        Err(e) => {
            return Posture::Unverified {
                reason: format!("malformed {}: {e}", paths.signed.display()),
            };
        }
    };

    let baseline = read_baseline(&paths.baseline);
    let state = trust_state(&genesis_pub, &baseline);

    match signed.verify(&state) {
        Ok(_) => {
            let canonical_hash = signed.payload.canonical_blake3();
            // Interpret the signed member set BEFORE ratcheting the durable
            // floor. Partial adoption is forbidden: admission and the future
            // router must never disagree about which records one epoch means.
            let routing_view =
                match strict_routing_view(&signed.payload.members, &signed.payload.subnet) {
                    Ok(view) => view,
                    Err(reason) => {
                        return Posture::Unverified {
                            reason: format!(
                                "verified inventory has unusable routing view: {reason}"
                            ),
                        };
                    }
                };
            // Persist the advanced rollback floor BEFORE declaring the
            // inventory authoritative. If we can't make the floor durable,
            // fail CLOSED: accepting now but losing the floor on the next
            // restart would reopen the rollback window (§6.4) — exactly the
            // attack the floor exists to stop.
            let accepted = match compare_and_ratchet_baseline(
                &paths.baseline,
                &signed,
                &genesis_pub,
                &canonical_hash,
            ) {
                Ok(accepted) => accepted,
                Err(e) => {
                    return Posture::Unverified {
                        reason: format!(
                            "inventory verified but the rollback baseline could not be safely ratcheted at {} ({e}); \
                             refusing authority rather than lose rollback protection across a restart",
                            paths.baseline.display()
                        ),
                    };
                }
            };
            Posture::Verified(Accepted {
                epoch: accepted.epoch,
                recovery_generation: accepted.recovery_generation,
                via_recovery: accepted.via_recovery,
                mesh: signed.payload.mesh.clone(),
                hash: canonical_hash,
                verified_by: accepted.verified_by,
                verify_keys: accepted
                    .adopt_verify_keys
                    .iter()
                    .map(|k| VerifyKeySummary {
                        key_id: k.key_id.clone(),
                        status: key_status_str(k.status).to_string(),
                    })
                    .collect(),
                members: summarize_members(&signed.payload.members),
                routing_view,
                members_full: members_by_name(&signed.payload.members),
            })
        }
        Err(e) => Posture::Unverified {
            reason: format!("verification failed: {e}"),
        },
    }
}

fn key_status_str(s: KeyStatus) -> &'static str {
    match s {
        KeyStatus::Active => "active",
        KeyStatus::Retiring => "retiring",
    }
}

fn read_genesis_pub(path: &Path) -> Result<Vec<u8>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        format!(
            "no genesis verify key at {} (§7.4 provisioning): {e}",
            path.display()
        )
    })?;
    let bytes = B64.decode(text.trim()).map_err(|e| {
        format!(
            "genesis verify key at {} is not base64: {e}",
            path.display()
        )
    })?;
    if bytes.len() != ED25519_PUBKEY_LEN {
        return Err(format!(
            "genesis verify key at {} is {} bytes, expected {ED25519_PUBKEY_LEN}",
            path.display(),
            bytes.len()
        ));
    }
    Ok(bytes)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Baseline {
    epoch: u64,
    recovery_generation: u64,
    hash: Option<String>,
}

fn trust_state(genesis_pub: &[u8], baseline: &Baseline) -> NodeTrustState {
    NodeTrustState {
        genesis_key_id: GENESIS_KEY_ID.into(),
        trusted_keys: vec![TrustedKey {
            key_id: GENESIS_KEY_ID.into(),
            pubkey: genesis_pub.to_vec(),
            status: KeyStatus::Active,
        }],
        last_epoch: baseline.epoch,
        last_recovery_generation: baseline.recovery_generation,
        last_canonical_hash: baseline.hash.clone(),
    }
}

/// Read the persisted `(epoch, recovery_generation, hash)` baseline. Numeric
/// values never seen are `0` (§6.4); an absent hash is the one-time migration
/// state from the legacy two-field format. A missing/unreadable file remains the
/// cold-boot baseline.
fn read_baseline(path: &Path) -> Baseline {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Baseline::default();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Baseline::default();
    };
    let epoch = v
        .get("epoch")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let recovery_gen = v
        .get("recovery_generation")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let hash = v
        .get("hash")
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string);
    Baseline {
        epoch,
        recovery_generation: recovery_gen,
        hash,
    }
}

/// Serialise every baseline update across noded processes. Lock acquisition is
/// bounded so a wedged peer process cannot prevent this noded from reaching its
/// listener: contention returns an error and therefore an `Unverified` posture.
/// Verification is repeated against the floor read while holding the lock, so
/// a delayed writer can never overwrite a later fold (or a different hash at
/// the same fold).
fn compare_and_ratchet_baseline(
    path: &Path,
    signed: &SignedInventory,
    genesis_pub: &[u8],
    canonical_hash: &str,
) -> Result<AcceptedInventory, String> {
    let Some(parent) = path.parent() else {
        return Err("baseline path has no parent directory".into());
    };
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("creating baseline directory {}: {e}", parent.display()))?;

    let lock_path = appended_path(path, ".lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| format!("opening baseline lock {}: {e}", lock_path.display()))?;
    acquire_baseline_lock(&lock_file, &lock_path)?;

    let fresh = read_baseline(path);
    let accepted = signed
        .verify(&trust_state(genesis_pub, &fresh))
        .map_err(|e| format!("fresh-floor verification failed under lock: {e}"))?;
    write_baseline_file(
        path,
        accepted.epoch,
        accepted.recovery_generation,
        canonical_hash,
    )
    .map_err(|e| format!("durable baseline write failed: {e}"))?;
    Ok(accepted)
}

fn acquire_baseline_lock(lock_file: &std::fs::File, lock_path: &Path) -> Result<(), String> {
    let started = Instant::now();
    loop {
        match lock_file.try_lock() {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::WouldBlock) => {
                let elapsed = started.elapsed();
                if elapsed >= BASELINE_LOCK_TIMEOUT {
                    return Err(format!(
                        "baseline lock contended at {} for {}ms; giving up without granting authority",
                        lock_path.display(),
                        elapsed.as_millis()
                    ));
                }
                std::thread::sleep(
                    BASELINE_LOCK_RETRY.min(BASELINE_LOCK_TIMEOUT.saturating_sub(elapsed)),
                );
            }
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(format!(
                    "trying baseline lock {} failed: {e}",
                    lock_path.display()
                ));
            }
        }
    }
}

fn appended_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

/// Persist the baseline crash-durably: write a unique temp file, **fsync it**,
/// atomic-rename it into place, then **fsync the parent directory** so the
/// rename itself survives a power loss. The caller holds the baseline lock.
fn write_baseline_file(
    path: &Path,
    epoch: u64,
    recovery_generation: u64,
    hash: &str,
) -> std::io::Result<()> {
    use std::io::Write as _;

    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "baseline path has no parent directory",
        )
    })?;
    std::fs::create_dir_all(parent)?;
    let body = serde_json::json!({
        "epoch": epoch,
        "recovery_generation": recovery_generation,
        "hash": hash,
    })
    .to_string()
        + "\n";

    let nonce = BASELINE_TMP_NONCE.fetch_add(1, Ordering::Relaxed);
    let tmp = appended_path(path, &format!(".{}.{}.tmp", std::process::id(), nonce));
    let result = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(body.as_bytes())?;
        f.sync_all()?; // fsync the data before the rename can expose it
        std::fs::rename(&tmp, path)?;
        sync_directory(parent)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn sync_directory(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

fn summarize_members(members: &serde_json::Value) -> Vec<MemberSummary> {
    members
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            Some(MemberSummary {
                name: m.get("name")?.as_str()?.to_string(),
                mesh_ip: m
                    .get("mesh_ip")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                bus: m
                    .get("bus")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
                status: m
                    .get("status")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .collect()
}

/// The full verified member records keyed by `name` — retained so `admit()`
/// (§9a, 2-c-1b) can hand the credential-bearing record to `select_d2_pubkeys`.
/// Built once at verify time from the SAME strictly validated `payload.members`
/// as the routing view. Admission consumes the full records while live route
/// lookup consumes the typed classification.
fn members_by_name(members: &serde_json::Value) -> BTreeMap<String, serde_json::Value> {
    members
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let name = m.get("name")?.as_str()?.to_string();
            Some((name, m.clone()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_mesh_trust::inventory::{
        ALG_ED25519, CANONICAL_ENCODING_V1, InvSignature, InventoryPayload, VerifyKey,
    };
    use ed25519_dalek::{Signer as _, SigningKey};

    fn write(path: &Path, body: &str) {
        std::fs::write(path, body).unwrap();
    }

    /// A throwaway temp dir unique to a test (no external rng — uses the
    /// test's own name as the suffix).
    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("noded_authority_{tag}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn paths_in(dir: &Path) -> AuthorityPaths {
        AuthorityPaths {
            genesis_pub: dir.join("genesis.pub"),
            signed: dir.join("inventory.signed"),
            baseline: dir.join("inventory.baseline"),
        }
    }

    fn write_signed_inventory(
        paths: &AuthorityPaths,
        signing_key: &SigningKey,
        epoch: u64,
        members: serde_json::Value,
    ) -> String {
        let pubkey = B64.encode(signing_key.verifying_key().to_bytes());
        let payload = InventoryPayload {
            schema_version: 1,
            canonical_encoding: CANONICAL_ENCODING_V1.into(),
            mesh: "example.internal".into(),
            subnet: "192.0.2.0/24".into(),
            epoch,
            signed_at: "2026-06-03T00:00:00Z".into(),
            valid_until: "2026-09-01T00:00:00Z".into(),
            hub: vec!["beta".into()],
            verify_keys: vec![VerifyKey {
                key_id: GENESIS_KEY_ID.into(),
                pubkey: pubkey.clone(),
                key_type: ALG_ED25519.into(),
                status: KeyStatus::Active,
            }],
            members,
            recovery: None,
            recovery_generation: None,
        };
        let hash = payload.canonical_blake3();
        let signature = signing_key.sign(&payload.canonical_bytes());
        let envelope = SignedInventory {
            signatures: vec![InvSignature {
                key_id: GENESIS_KEY_ID.into(),
                alg: ALG_ED25519.into(),
                sig: B64.encode(signature.to_bytes()),
            }],
            payload,
        };
        write(&paths.genesis_pub, &pubkey);
        write(&paths.signed, &serde_json::to_string(&envelope).unwrap());
        hash
    }

    #[test]
    fn unverified_when_no_genesis_key() {
        let dir = tmpdir("nogen");
        let p = paths_in(&dir);
        match load_and_verify(&p) {
            Posture::Unverified { reason } => assert!(reason.contains("genesis")),
            other => panic!("expected Unverified, got {other:?}"),
        }
    }

    #[test]
    fn unverified_when_no_signed_cache() {
        let dir = tmpdir("nocache");
        let p = paths_in(&dir);
        write(&p.genesis_pub, &B64.encode([0u8; 32]));
        match load_and_verify(&p) {
            Posture::Unverified { reason } => assert!(reason.contains("no signed inventory")),
            other => panic!("expected Unverified, got {other:?}"),
        }
    }

    #[test]
    fn read_baseline_defaults_to_zero() {
        let dir = tmpdir("baseline");
        assert_eq!(read_baseline(&dir.join("missing")), Baseline::default());
        let bp = dir.join("b.json");
        write(&bp, r#"{"epoch": 7, "recovery_generation": 2}"#);
        assert_eq!(
            read_baseline(&bp),
            Baseline {
                epoch: 7,
                recovery_generation: 2,
                hash: None,
            }
        );
        // corrupt → cold-boot default, never a panic
        write(&bp, "not json");
        assert_eq!(read_baseline(&bp), Baseline::default());
    }

    #[test]
    fn write_then_read_baseline_round_trips_atomically() {
        let dir = tmpdir("bwrite");
        let bp = dir.join("inventory.baseline");
        write_baseline_file(&bp, 42, 3, "abc123").unwrap();
        assert_eq!(
            read_baseline(&bp),
            Baseline {
                epoch: 42,
                recovery_generation: 3,
                hash: Some("abc123".into()),
            }
        );
        // no unique temp file is left behind
        assert!(std::fs::read_dir(&dir).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
    }

    #[test]
    fn write_baseline_surfaces_io_errors() {
        // A regular file standing in for the parent dir makes both
        // create_dir_all and File::create fail — write_baseline_file must
        // return Err (which load_and_verify turns into fail-closed
        // Unverified, the BLOCKER fix).
        let dir = tmpdir("bwfail");
        let file_as_parent = dir.join("not_a_dir");
        write(&file_as_parent, "x");
        assert!(
            write_baseline_file(&file_as_parent.join("inventory.baseline"), 1, 0, "hash").is_err()
        );
    }

    #[test]
    fn directory_fsync_open_errors_are_propagated() {
        let dir = tmpdir("dirsyncfail");
        let error = sync_directory(&dir.join("missing-directory")).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn old_two_field_baseline_accepts_once_and_migrates_to_hash() {
        let dir = tmpdir("baseline_migration");
        let paths = paths_in(&dir);
        let signing_key = SigningKey::from_bytes(&[11u8; 32]);
        let hash = write_signed_inventory(
            &paths,
            &signing_key,
            7,
            serde_json::json!([{
                "name": "alpha", "mesh_ip": "192.0.2.5", "bus": true, "status": "active"
            }]),
        );
        write(&paths.baseline, r#"{"epoch":7,"recovery_generation":0}"#);

        assert!(matches!(load_and_verify(&paths), Posture::Verified(_)));
        assert_eq!(
            read_baseline(&paths.baseline),
            Baseline {
                epoch: 7,
                recovery_generation: 0,
                hash: Some(hash),
            }
        );
    }

    #[test]
    fn summarize_members_projects_validated_records() {
        let members = serde_json::json!([
            { "name": "alpha", "mesh_ip": "192.0.2.5", "bus": true, "status": "active" },
            { "name": "beta", "mesh_ip": "192.0.2.6", "bus": false, "status": "active" },
        ]);
        let s = summarize_members(&members);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].name, "alpha");
        assert!(s[0].bus);
        assert_eq!(s[1].name, "beta");
        assert!(!s[1].bus);
    }

    #[test]
    fn members_by_name_keys_and_retains_credentials() {
        let members = serde_json::json!([
            {
                "name": "delta", "mesh_ip": "192.0.2.5", "bus": true, "status": "active",
                "credentials": [{ "kind": "d2", "pubkey": "AAAA", "from_epoch": 1, "until_epoch": null }]
            },
            { "name": "retired", "mesh_ip": "192.0.2.6", "bus": true, "status": "tombstoned" },
        ]);
        let m = members_by_name(&members);
        assert_eq!(m.len(), 2);
        let delta = m.get("delta").expect("delta retained by name");
        assert_eq!(delta["credentials"][0]["kind"], "d2");
        assert_eq!(delta["credentials"][0]["pubkey"], "AAAA");
    }

    #[test]
    fn malformed_routing_view_rejects_before_baseline_ratchet() {
        let dir = tmpdir("view_before_ratchet");
        let paths = paths_in(&dir);
        let signing_key = SigningKey::from_bytes(&[12u8; 32]);
        write_baseline_file(&paths.baseline, 1, 0, "old-hash").unwrap();
        let before = std::fs::read(&paths.baseline).unwrap();
        write_signed_inventory(
            &paths,
            &signing_key,
            2,
            serde_json::json!([
                { "name": "alpha", "mesh_ip": "192.0.2.5", "bus": true, "status": "active" },
                { "name": "beta", "mesh_ip": "not-an-ip", "bus": true, "status": "active" }
            ]),
        );

        let Posture::Unverified { reason } = load_and_verify(&paths) else {
            panic!("malformed routing view must reject");
        };
        assert!(reason.contains("member[1]: invalid mesh_ip \"not-an-ip\""));
        assert_eq!(std::fs::read(&paths.baseline).unwrap(), before);
        assert_eq!(
            read_baseline(&paths.baseline),
            Baseline {
                epoch: 1,
                recovery_generation: 0,
                hash: Some("old-hash".into()),
            }
        );
    }

    #[test]
    fn equal_epoch_different_hash_rejects_without_rewriting_baseline() {
        let dir = tmpdir("equal_epoch_hash_conflict");
        let paths = paths_in(&dir);
        let signing_key = SigningKey::from_bytes(&[13u8; 32]);
        let first_hash = write_signed_inventory(
            &paths,
            &signing_key,
            7,
            serde_json::json!([{
                "name": "alpha", "mesh_ip": "192.0.2.5", "bus": true, "status": "active"
            }]),
        );
        assert!(matches!(load_and_verify(&paths), Posture::Verified(_)));
        let pinned = read_baseline(&paths.baseline);
        assert_eq!(pinned.hash.as_deref(), Some(first_hash.as_str()));

        let conflicting_hash = write_signed_inventory(
            &paths,
            &signing_key,
            7,
            serde_json::json!([{
                "name": "beta", "mesh_ip": "192.0.2.6", "bus": true, "status": "active"
            }]),
        );
        assert_ne!(conflicting_hash, first_hash);
        let Posture::Unverified { reason } = load_and_verify(&paths) else {
            panic!("same fold with different hash must reject");
        };
        assert!(reason.contains("conflicts with the accepted baseline hash"));
        assert_eq!(read_baseline(&paths.baseline), pinned);
    }

    #[test]
    fn concurrent_compare_and_ratchet_never_regresses_the_floor() {
        use std::sync::{Arc, Barrier};

        let dir = tmpdir("concurrent_ratchet");
        let paths = paths_in(&dir);
        let signing_key = SigningKey::from_bytes(&[14u8; 32]);
        let members = serde_json::json!([{
            "name": "alpha", "mesh_ip": "192.0.2.5", "bus": true, "status": "active"
        }]);

        let low_hash = write_signed_inventory(&paths, &signing_key, 2, members.clone());
        let low = SignedInventory::parse(&std::fs::read(&paths.signed).unwrap()).unwrap();
        let high_hash = write_signed_inventory(&paths, &signing_key, 3, members);
        let high = SignedInventory::parse(&std::fs::read(&paths.signed).unwrap()).unwrap();
        let genesis_pub = signing_key.verifying_key().to_bytes().to_vec();
        let barrier = Arc::new(Barrier::new(2));

        let run = |signed: SignedInventory, hash: String, barrier: Arc<Barrier>| {
            let baseline = paths.baseline.clone();
            let genesis_pub = genesis_pub.clone();
            std::thread::spawn(move || {
                barrier.wait();
                compare_and_ratchet_baseline(&baseline, &signed, &genesis_pub, &hash)
            })
        };
        let low_result = run(low, low_hash, Arc::clone(&barrier));
        let high_result = run(high, high_hash.clone(), barrier);
        let _ = low_result.join().unwrap();
        assert!(high_result.join().unwrap().is_ok());

        assert_eq!(
            read_baseline(&paths.baseline),
            Baseline {
                epoch: 3,
                recovery_generation: 0,
                hash: Some(high_hash),
            }
        );
    }

    #[test]
    fn held_baseline_lock_yields_unverified_within_bound() {
        let dir = tmpdir("held_baseline_lock");
        let paths = paths_in(&dir);
        let signing_key = SigningKey::from_bytes(&[15u8; 32]);
        write_signed_inventory(
            &paths,
            &signing_key,
            1,
            serde_json::json!([{
                "name": "alpha", "mesh_ip": "192.0.2.5", "bus": true, "status": "active"
            }]),
        );

        let lock_path = appended_path(&paths.baseline, ".lock");
        let held_lock = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        held_lock.lock().unwrap();

        let started = Instant::now();
        let posture = load_and_verify(&paths);
        let elapsed = started.elapsed();

        let Posture::Unverified { reason } = posture else {
            panic!("contended baseline lock must not grant authority");
        };
        assert!(reason.contains("baseline lock contended"), "{reason}");
        assert!(
            elapsed >= BASELINE_LOCK_TIMEOUT,
            "lock contention returned before the configured retry window: {elapsed:?}"
        );
        assert!(
            elapsed < BASELINE_LOCK_TIMEOUT + Duration::from_secs(1),
            "lock contention exceeded its bounded acquisition window: {elapsed:?}"
        );
    }

    #[test]
    fn accepted_authority_hash_matches_pre_refactor_golden() {
        let dir = tmpdir("canonical_hash_golden");
        let paths = paths_in(&dir);
        let signing_key = SigningKey::from_bytes(&[7u8; 32]);
        let pubkey = B64.encode(signing_key.verifying_key().to_bytes());
        let payload = InventoryPayload {
            schema_version: 1,
            canonical_encoding: CANONICAL_ENCODING_V1.into(),
            mesh: "example.internal".into(),
            subnet: "192.0.2.0/24".into(),
            epoch: 7,
            signed_at: "2026-06-03T00:00:00Z".into(),
            valid_until: "2026-09-01T00:00:00Z".into(),
            hub: vec!["beta".into()],
            verify_keys: vec![VerifyKey {
                key_id: GENESIS_KEY_ID.into(),
                pubkey: pubkey.clone(),
                key_type: ALG_ED25519.into(),
                status: KeyStatus::Active,
            }],
            members: serde_json::json!([{
                "name": "alpha",
                "mesh_ip": "192.0.2.5",
                "bus": true,
                "status": "active",
            }]),
            recovery: None,
            recovery_generation: None,
        };
        let signature = signing_key.sign(&payload.canonical_bytes());
        let envelope = SignedInventory {
            signatures: vec![InvSignature {
                key_id: GENESIS_KEY_ID.into(),
                alg: ALG_ED25519.into(),
                sig: B64.encode(signature.to_bytes()),
            }],
            payload,
        };
        write(&paths.genesis_pub, &pubkey);
        write(&paths.signed, &serde_json::to_string(&envelope).unwrap());

        let Posture::Verified(accepted) = load_and_verify(&paths) else {
            panic!("golden inventory should verify");
        };
        assert_eq!(
            accepted.hash,
            "bdc14acb1757ad734081c9a6d437b6e56f7abfe4e156ffc45cfe1cedf861a60e"
        );
    }
}
