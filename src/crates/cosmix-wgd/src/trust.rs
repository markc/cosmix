//! Load + verify this node's signed mesh inventory — the source of truth wgd
//! derives its peer set from (SPEC-13 §7.1 INV-1).
//!
//! This mirrors `cosmix-noded::authority` exactly (same files, same
//! `genesis_key_id = "genesis"`, same base-64 anchor format), because wgd and
//! noded MUST agree byte-for-byte on what "verified" means. The one difference:
//! **wgd is a read-only consumer of the trust state.** It reads the same
//! `/etc/cosmix/noded/genesis.pub` anchor and the same
//! `/var/lib/cosmix/noded/inventory.baseline` epoch/hash floor, but it NEVER writes
//! the baseline — noded owns the rollback-floor advance. wgd honours the floor
//! on read so it cannot be fed a rolled-back cache that noded would reject.
//!
//! The node MUST have `verify()`-ed before wgd reads `payload.members`
//! ([`crate::derive`]); this module is that gate.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use cosmix_mesh_trust::inventory::{
    AcceptedInventory, KeyStatus, NodeTrustState, SignedInventory, TrustedKey, VerifyError,
};
use cosmix_mesh_trust::routing::{RoutingMember, RoutingViewError, strict_routing_view};

/// The genesis anchor's key_id — a fixed label (not a hash), matched by string
/// equality against the inventory's `verify_keys[]`/`signatures[]`. Identical
/// to `cosmix-noded::authority::GENESIS_KEY_ID` and the signer's constant.
const GENESIS_KEY_ID: &str = "genesis";
/// Ed25519 verify keys are 32 bytes.
const ED25519_PUBKEY_LEN: usize = 32;
const BASELINE_STABILITY_ATTEMPTS: usize = 3;

/// Canonical on-disk locations, owned by noded (SPEC-13 plane-A cache). wgd
/// reads them; provisioning/noded write them.
#[derive(Debug, Clone)]
pub struct TrustPaths {
    /// The genesis trust anchor: a bare base-64 Ed25519 pubkey + newline.
    pub genesis_pub: PathBuf,
    /// The enveloped signed inventory (`{payload, signatures}`) — the INV-1
    /// last-known-good cache noded verifies and hot-reloads.
    pub signed: PathBuf,
    /// The persisted `{epoch, recovery_generation, hash}` rollback floor. Read-only
    /// for wgd; noded owns the write.
    pub baseline: PathBuf,
}

impl Default for TrustPaths {
    fn default() -> Self {
        Self {
            genesis_pub: PathBuf::from("/etc/cosmix/noded/genesis.pub"),
            signed: PathBuf::from("/var/lib/cosmix/noded/inventory.signed"),
            baseline: PathBuf::from("/var/lib/cosmix/noded/inventory.baseline"),
        }
    }
}

/// A verified inventory + the acceptance facts. Hand `signed.payload` to
/// [`crate::derive::derive_intended`] only via this type — its existence
/// proves `verify()` succeeded.
#[derive(Debug, Clone)]
pub struct VerifiedInventory {
    pub signed: SignedInventory,
    pub epoch: u64,
    pub via_recovery: bool,
    pub verified_by: Vec<String>,
    /// The same strict signed-membership interpretation noded accepted.
    pub routing_view: Vec<RoutingMember>,
}

/// Everything that can stop wgd trusting the on-disk inventory.
#[derive(Debug, thiserror::Error)]
pub enum TrustError {
    #[error("reading genesis anchor {path}: {source}")]
    GenesisRead {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("genesis anchor {path} is not valid base64")]
    GenesisDecode { path: PathBuf },
    #[error("genesis anchor {path} decoded to {got} bytes, expected {ED25519_PUBKEY_LEN}")]
    GenesisWrongLen { path: PathBuf, got: usize },
    #[error("reading signed inventory {path}: {source}")]
    SignedRead {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("parsing signed inventory {path}: {source}")]
    SignedParse {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("verifying signed inventory {path}: {source}")]
    Verify { path: PathBuf, source: VerifyError },
    #[error("deriving routing view from signed inventory {path}: {source}")]
    RoutingView {
        path: PathBuf,
        source: RoutingViewError,
    },
    #[error(
        "reading rollback-floor baseline {path}: {source} (failing closed — an unreadable floor could let a rolled-back inventory through)"
    )]
    BaselineRead {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("parsing rollback-floor baseline {path}: {source} (failing closed)")]
    BaselineParse {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("rollback-floor baseline {path} has no numeric `epoch` (failing closed)")]
    BaselineMalformed { path: PathBuf },
    #[error(
        "rollback-floor baseline {path} changed during verification {attempts} times; failing closed"
    )]
    BaselineChanged { path: PathBuf, attempts: usize },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Baseline {
    epoch: u64,
    recovery_generation: u64,
    hash: Option<String>,
}

/// Read the genesis anchor: a bare base-64 Ed25519 pubkey (trailing newline
/// tolerated), decoded to its 32 raw bytes. Same parse as
/// `authority::read_genesis_pub`.
fn read_genesis_pub(path: &Path) -> Result<Vec<u8>, TrustError> {
    let text = std::fs::read_to_string(path).map_err(|source| TrustError::GenesisRead {
        path: path.to_path_buf(),
        source,
    })?;
    let bytes = B64
        .decode(text.trim())
        .map_err(|_| TrustError::GenesisDecode {
            path: path.to_path_buf(),
        })?;
    if bytes.len() != ED25519_PUBKEY_LEN {
        return Err(TrustError::GenesisWrongLen {
            path: path.to_path_buf(),
            got: bytes.len(),
        });
    }
    Ok(bytes)
}

/// Read the persisted rollback floor `{epoch, recovery_generation, hash}`.
///
/// **Fail-closed for a read-only consumer (Codex 2026-07-06).** noded's own
/// `read_baseline` defaults any failure to `(0, 0, None)` because noded also *writes*
/// the floor and is the authority; wgd only *reads* it, so it must not treat an
/// unreadable/corrupt floor as "epoch 0". Otherwise a readable-but-rolled-back
/// `inventory.signed` plus an unreadable `baseline` (e.g. a permission miss)
/// would let wgd accept a rollback that noded — with its persisted floor —
/// rejects. So:
/// - **missing file** (`NotFound`) → `(0, 0, None)` cold-boot default (legitimate:
///   no floor has been established yet, matching noded's first boot);
/// - **any other IO error** (notably `PermissionDenied`) → fail closed;
/// - **corrupt/malformed JSON, or no numeric `epoch`** → fail closed.
///
/// Read-only: wgd never writes the baseline; noded owns the advance.
fn read_baseline(path: &Path) -> Result<Baseline, TrustError> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Baseline::default()),
        Err(source) => {
            return Err(TrustError::BaselineRead {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|source| TrustError::BaselineParse {
            path: path.to_path_buf(),
            source,
        })?;
    let epoch = v
        .get("epoch")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| TrustError::BaselineMalformed {
            path: path.to_path_buf(),
        })?;
    // recovery_generation absent → 0 (a floor written before recovery existed).
    let rec = v
        .get("recovery_generation")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    // hash absent → one-time migration from noded's legacy two-field floor.
    let hash = v
        .get("hash")
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string);
    Ok(Baseline {
        epoch,
        recovery_generation: rec,
        hash,
    })
}

fn trust_state(genesis_pub: &[u8], baseline: &Baseline) -> NodeTrustState {
    NodeTrustState {
        genesis_key_id: GENESIS_KEY_ID.to_string(),
        trusted_keys: vec![TrustedKey {
            key_id: GENESIS_KEY_ID.to_string(),
            pubkey: genesis_pub.to_vec(),
            status: KeyStatus::Active,
        }],
        last_epoch: baseline.epoch,
        last_recovery_generation: baseline.recovery_generation,
        last_canonical_hash: baseline.hash.clone(),
    }
}

fn verify_with_stable_baseline<F>(
    signed: &SignedInventory,
    genesis_pub: &[u8],
    signed_path: &Path,
    baseline_path: &Path,
    mut read: F,
) -> Result<(AcceptedInventory, Vec<RoutingMember>), TrustError>
where
    F: FnMut() -> Result<Baseline, TrustError>,
{
    for attempt in 1..=BASELINE_STABILITY_ATTEMPTS {
        let before = read()?;
        let accepted = signed
            .verify(&trust_state(genesis_pub, &before))
            .map_err(|source| TrustError::Verify {
                path: signed_path.to_path_buf(),
                source,
            })?;
        let routing_view = strict_routing_view(&signed.payload.members, &signed.payload.subnet)
            .map_err(|source| TrustError::RoutingView {
                path: signed_path.to_path_buf(),
                source,
            })?;
        let after = read()?;
        if before == after {
            return Ok((accepted, routing_view));
        }
        if attempt == BASELINE_STABILITY_ATTEMPTS {
            return Err(TrustError::BaselineChanged {
                path: baseline_path.to_path_buf(),
                attempts: BASELINE_STABILITY_ATTEMPTS,
            });
        }
    }
    unreachable!("the bounded baseline-stability loop always returns")
}

/// Build the node trust state from the genesis anchor + persisted floor, then
/// parse and verify the on-disk signed inventory. Mirrors
/// `authority::load_and_verify` (minus the baseline WRITE — wgd is read-only).
pub fn load_and_verify(paths: &TrustPaths) -> Result<VerifiedInventory, TrustError> {
    let genesis_pub = read_genesis_pub(&paths.genesis_pub)?;
    let wire = std::fs::read(&paths.signed).map_err(|source| TrustError::SignedRead {
        path: paths.signed.clone(),
        source,
    })?;
    let signed = SignedInventory::parse(&wire).map_err(|source| TrustError::SignedParse {
        path: paths.signed.clone(),
        source,
    })?;
    let (accepted, routing_view) = verify_with_stable_baseline(
        &signed,
        &genesis_pub,
        &paths.signed,
        &paths.baseline,
        || read_baseline(&paths.baseline),
    )?;

    Ok(VerifiedInventory {
        signed,
        epoch: accepted.epoch,
        via_recovery: accepted.via_recovery,
        verified_by: accepted.verified_by,
        routing_view,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_mesh_trust::inventory::{
        ALG_ED25519, CANONICAL_ENCODING_V1, InvSignature, InventoryPayload, VerifyKey,
    };
    use ed25519_dalek::{Signer as _, SigningKey};
    use std::collections::VecDeque;

    fn signed_inventory(members: serde_json::Value) -> (Vec<u8>, SignedInventory) {
        let signing_key = SigningKey::from_bytes(&[21u8; 32]);
        let public = signing_key.verifying_key().to_bytes().to_vec();
        let public_b64 = B64.encode(&public);
        let payload = InventoryPayload {
            schema_version: 1,
            canonical_encoding: CANONICAL_ENCODING_V1.into(),
            mesh: "example.internal".into(),
            subnet: "192.0.2.0/24".into(),
            epoch: 7,
            signed_at: "2026-06-03T00:00:00Z".into(),
            valid_until: "2026-09-01T00:00:00Z".into(),
            hub: vec!["alpha".into()],
            verify_keys: vec![VerifyKey {
                key_id: GENESIS_KEY_ID.into(),
                pubkey: public_b64,
                key_type: ALG_ED25519.into(),
                status: KeyStatus::Active,
            }],
            members,
            recovery: None,
            recovery_generation: None,
        };
        let signature = signing_key.sign(&payload.canonical_bytes());
        let signed = SignedInventory {
            signatures: vec![InvSignature {
                key_id: GENESIS_KEY_ID.into(),
                alg: ALG_ED25519.into(),
                sig: B64.encode(signature.to_bytes()),
            }],
            payload,
        };
        (public, signed)
    }

    fn valid_members() -> serde_json::Value {
        serde_json::json!([{
            "name": "alpha", "mesh_ip": "192.0.2.5", "bus": true, "status": "active"
        }])
    }

    #[test]
    fn stable_baseline_retry_rechecks_against_a_new_hash_pin() {
        let (genesis_pub, signed) = signed_inventory(valid_members());
        let mut reads = VecDeque::from([
            Baseline {
                epoch: 7,
                recovery_generation: 0,
                hash: None,
            },
            Baseline {
                epoch: 7,
                recovery_generation: 0,
                hash: Some("different-pinned-hash".into()),
            },
            Baseline {
                epoch: 7,
                recovery_generation: 0,
                hash: Some("different-pinned-hash".into()),
            },
        ]);
        let error = verify_with_stable_baseline(
            &signed,
            &genesis_pub,
            Path::new("inventory.signed"),
            Path::new("inventory.baseline"),
            || Ok(reads.pop_front().expect("expected baseline read")),
        )
        .expect_err("the fresh pinned hash must be authoritative");
        assert!(matches!(
            error,
            TrustError::Verify {
                source: VerifyError::BaselineHashMismatch { .. },
                ..
            }
        ));
    }

    #[test]
    fn changing_baseline_exhausts_retry_and_fails_closed() {
        let (genesis_pub, signed) = signed_inventory(valid_members());
        let mut reads = VecDeque::from([
            Baseline::default(),
            Baseline {
                epoch: 1,
                ..Baseline::default()
            },
            Baseline {
                epoch: 1,
                ..Baseline::default()
            },
            Baseline {
                epoch: 2,
                ..Baseline::default()
            },
            Baseline {
                epoch: 2,
                ..Baseline::default()
            },
            Baseline {
                epoch: 3,
                ..Baseline::default()
            },
        ]);
        let error = verify_with_stable_baseline(
            &signed,
            &genesis_pub,
            Path::new("inventory.signed"),
            Path::new("inventory.baseline"),
            || Ok(reads.pop_front().expect("expected baseline read")),
        )
        .expect_err("a perpetually moving floor must fail closed");
        assert!(matches!(
            error,
            TrustError::BaselineChanged { attempts: 3, .. }
        ));
    }

    #[test]
    fn wgd_rejects_the_shared_semantically_unusable_routing_view() {
        let (genesis_pub, signed) = signed_inventory(serde_json::json!([{
            "name": "alpha", "mesh_ip": "198.51.100.5", "bus": true, "status": "active"
        }]));
        let error = verify_with_stable_baseline(
            &signed,
            &genesis_pub,
            Path::new("inventory.signed"),
            Path::new("inventory.baseline"),
            || Ok(Baseline::default()),
        )
        .expect_err("wgd must reject the same route view as noded");
        assert!(matches!(error, TrustError::RoutingView { .. }));
        assert!(error.to_string().contains("outside inventory subnet"));
    }

    #[test]
    fn wgd_passes_through_valid_signed_port_and_rejects_malformed_port() {
        let (genesis_pub, signed) = signed_inventory(serde_json::json!([{
            "name": "alpha", "mesh_ip": "192.0.2.5", "bus": true,
            "status": "active", "noded_port": 4300
        }]));
        let (_, view) = verify_with_stable_baseline(
            &signed,
            &genesis_pub,
            Path::new("inventory.signed"),
            Path::new("inventory.baseline"),
            || Ok(Baseline::default()),
        )
        .expect("valid signed endpoint");
        assert!(matches!(
            view[0],
            RoutingMember::ActiveBus {
                noded_port: 4300,
                ..
            }
        ));

        let (genesis_pub, signed) = signed_inventory(serde_json::json!([{
            "name": "alpha", "mesh_ip": "192.0.2.5", "bus": true,
            "status": "active", "noded_port": 0
        }]));
        let error = verify_with_stable_baseline(
            &signed,
            &genesis_pub,
            Path::new("inventory.signed"),
            Path::new("inventory.baseline"),
            || Ok(Baseline::default()),
        )
        .expect_err("malformed signed endpoint must reject the whole view");
        assert!(matches!(error, TrustError::RoutingView { .. }));
        assert!(error.to_string().contains("noded_port"));
    }
}
