//! SPEC 13 §9a D2 broker-admission — the broker-side handshake state (slice
//! 2-c-1b). The crypto/membership gate lives in
//! [`cosmix_mesh_trust::admission`] (`admit`, `AdmissionTranscript`,
//! `canonical_bytes`); this module is the broker's *session state* around it:
//! the outstanding-challenge table, the wire frames, and the byte-for-byte
//! reconstruction of the transcript from the response.
//!
//! **The byte-for-byte hazard (§9a, design §5.1).** The signer and verifier
//! share `canonical_bytes()`, so the ONLY place they can diverge is in how each
//! side populates the [`AdmissionTranscript`] from the wire. We therefore build
//! the transcript from the **stored challenge raw bytes** (session_id,
//! server_nonce — never re-decoded from the wire) and the response's
//! base64-decoded raw bytes / native-`u64` epoch — never the base64 strings.
//! The [`tests::round_trip_transcript_is_byte_identical`] test pins this.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use base64::Engine as _;
use cosmix_mesh_trust::admission::{ADMIT_DOMAIN_V1, AdmissionError, AdmissionTranscript};
use serde_json::Value;
use tokio::sync::RwLock;

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// Origin-only profile (the testbed target): `client_ephemeral` and
/// `channel_binding_hash` are fixed-length 32 zero bytes on both sides.
const ORIGIN_ONLY_ZERO: [u8; 32] = [0u8; 32];

/// A challenge in flight on one accepted socket. Single-use: removed on the
/// matching response (or on socket close / TTL). The raw `session_id` /
/// `server_nonce` are kept as bytes so the transcript is rebuilt from them, not
/// re-decoded from the response wire.
#[derive(Debug, Clone)]
pub struct OutstandingChallenge {
    pub session_id: Vec<u8>,
    pub server_nonce: Vec<u8>,
    /// The broker's accepted epoch at challenge time — the client signs against
    /// this; `admit()` re-checks it against the broker's *current* epoch.
    pub epoch: u64,
    issued_at: Instant,
}

/// One challenge per accepted socket, keyed by the Bus correlation `id`.
/// **Bounded** (design §5.1 DoS): a global cap + a TTL + remove-on-response /
/// reap-on-close, since broker-speaks-first allocates an entry on every socket.
pub struct ChallengeTable {
    map: RwLock<HashMap<String, OutstandingChallenge>>,
    next_id: AtomicU64,
}

/// Max concurrent outstanding challenges before `issue` refuses (a connect
/// flood from any WG-superset peer must not OOM the broker — the hub most of
/// all). A challenge is short-lived (issued→responded/closed), so this caps
/// only genuinely-pending handshakes.
const MAX_OUTSTANDING: usize = 4096;
/// A challenge a client never answers is reaped after this.
const CHALLENGE_TTL: Duration = Duration::from_secs(10);

/// The challenge a broker sends as its first frame on a freshly-accepted D2
/// socket (the body of a `noded.admit.challenge` request).
pub struct IssuedChallenge {
    /// The Bus `id` the response must echo.
    pub id: String,
    /// The JSON body of the `noded.admit.challenge` frame.
    pub body: String,
}

impl ChallengeTable {
    pub fn new() -> Self {
        Self {
            map: RwLock::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    /// Generate + record a challenge for an accepted session, returning the
    /// `noded.admit.challenge` body to send first. `None` if the broker is at
    /// the outstanding-challenge cap (fail safe: the session simply isn't
    /// challenged — in observe that means no verdict, never a refusal).
    pub async fn issue(
        &self,
        epoch: u64,
        mesh_fqdn: &str,
        verifying_broker_node: &str,
    ) -> Option<IssuedChallenge> {
        let session_id = random_bytes::<16>();
        let server_nonce = random_bytes::<32>();
        let n = self.next_id.fetch_add(1, Ordering::Relaxed);
        let id = format!("adm-{n}");

        let mut map = self.map.write().await;
        // Opportunistically reap expired entries so a steady connect rate of
        // never-answered challenges can't accumulate to the cap.
        let now = Instant::now();
        map.retain(|_, c| now.duration_since(c.issued_at) < CHALLENGE_TTL);
        if map.len() >= MAX_OUTSTANDING {
            return None;
        }
        let body = serde_json::json!({
            "admit_version": ADMIT_DOMAIN_V1,
            "mesh_fqdn": mesh_fqdn,
            "verifying_broker_node": verifying_broker_node,
            "inventory_epoch": epoch,
            "session_id": B64.encode(&session_id),
            "server_nonce": B64.encode(&server_nonce),
            "profile": "origin-only",
        })
        .to_string();
        map.insert(
            id.clone(),
            OutstandingChallenge {
                session_id,
                server_nonce,
                epoch,
                issued_at: now,
            },
        );
        Some(IssuedChallenge { id, body })
    }

    /// Take (remove) the outstanding challenge for `id` — single-use, under one
    /// write lock (mirrors `PendingResponseTable::take`). The entry is removed
    /// regardless (single-use), but a response that arrives past `CHALLENGE_TTL`
    /// returns `None` (treated as stale) — freshness MUST NOT depend on a later
    /// `issue()` happening to reap it. A second response, or one for an
    /// unknown/expired id, finds nothing → the caller records
    /// `stale-or-replayed-challenge`.
    pub async fn take(&self, id: &str) -> Option<OutstandingChallenge> {
        let c = self.map.write().await.remove(id)?;
        if Instant::now().duration_since(c.issued_at) >= CHALLENGE_TTL {
            None
        } else {
            Some(c)
        }
    }

    /// Reap on socket close — free a session's challenge entry if it never
    /// answered (it may have been `take`n already, in which case this is a
    /// no-op).
    pub async fn reap(&self, id: &str) {
        self.map.write().await.remove(id);
    }
}

impl Default for ChallengeTable {
    fn default() -> Self {
        Self::new()
    }
}

/// A reconstructed admission proof, ready for `admit()`.
#[derive(Debug)]
pub struct ReconstructedProof {
    pub transcript: AdmissionTranscript,
    pub claimed_source_node: String,
    pub signature: Vec<u8>,
}

/// A bad-wire-shape error before `admit()` is even reached (not a membership
/// verdict). The §9a `admit-version` is enforced implicitly — the domain tag is
/// baked into `canonical_bytes()`, so a wrong-version client's signature simply
/// fails to verify (`bad-credential-signature`). Maps to its §17 detail string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
    Malformed,
}

impl WireError {
    pub fn detail(&self) -> &'static str {
        match self {
            WireError::Malformed => "malformed-response",
        }
    }
}

/// Rebuild the [`AdmissionTranscript`] from the **stored** challenge (raw
/// session_id/server_nonce bytes) and the `noded.admit.response` body (base64
/// fields decoded to raw bytes; epoch from the stored challenge as native
/// `u64`). The broker supplies `mesh_fqdn`/`verifying_broker_node` from its own
/// accepted authority + identity, never from the wire. This is the single
/// codec boundary — see the module-level hazard note.
pub fn reconstruct_proof(
    challenge: &OutstandingChallenge,
    response_body: &Value,
    mesh_fqdn: &str,
    verifying_broker_node: &str,
) -> Result<ReconstructedProof, WireError> {
    let claimed = response_body
        .get("claimed_source_node")
        .and_then(Value::as_str)
        .ok_or(WireError::Malformed)?
        .to_string();
    let signature = decode_b64(response_body, "signature")?;
    // Origin-only: these MUST be exactly 32 zero bytes; decode + verify shape so
    // a hardened-profile or a malformed client is a clean reject, not a silent
    // transcript divergence.
    let client_ephemeral = decode_b64(response_body, "client_ephemeral")?;
    let channel_binding_hash = decode_b64(response_body, "channel_binding_hash")?;
    if client_ephemeral != ORIGIN_ONLY_ZERO || channel_binding_hash != ORIGIN_ONLY_ZERO {
        return Err(WireError::Malformed);
    }

    let transcript = AdmissionTranscript {
        mesh_fqdn: mesh_fqdn.to_string(),
        claimed_source_node: claimed.clone(),
        verifying_broker_node: verifying_broker_node.to_string(),
        inventory_epoch: challenge.epoch,
        session_id: challenge.session_id.clone(),
        server_nonce: challenge.server_nonce.clone(),
        client_ephemeral,
        channel_binding_hash,
    };
    Ok(ReconstructedProof {
        transcript,
        claimed_source_node: claimed,
        signature,
    })
}

/// blake3 of the canonical transcript bytes, hex-truncated — emitted in
/// `admission.observed` so a 100%-bad-sig observe rate is recognised as
/// transcript divergence, not "no d2 keys yet" (design §5.1).
pub fn canon_digest(transcript: &AdmissionTranscript) -> String {
    let h = blake3::hash(&transcript.canonical_bytes());
    h.to_hex()[..12].to_string()
}

/// The `AdmissionError`→§17 detail 1:1 map (design §5.6).
pub fn verdict_detail(err: &AdmissionError) -> &'static str {
    match err {
        AdmissionError::SourceTombstoned(_) => "source-tombstoned",
        AdmissionError::SourceBusFalse(_) => "source-bus-false",
        AdmissionError::NameMismatch { .. } => "name-mismatch",
        AdmissionError::EpochMismatch { .. } => "epoch-mismatch",
        AdmissionError::NoCurrentD2Credential(_) => "no-current-d2-credential",
        AdmissionError::BadCredentialSignature(_) => "bad-credential-signature",
        AdmissionError::MalformedMember(_) => "malformed-member",
    }
}

fn decode_b64(body: &Value, field: &str) -> Result<Vec<u8>, WireError> {
    let s = body
        .get(field)
        .and_then(Value::as_str)
        .ok_or(WireError::Malformed)?;
    B64.decode(s).map_err(|_| WireError::Malformed)
}

fn random_bytes<const N: usize>() -> Vec<u8> {
    let mut b = vec![0u8; N];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut b);
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_mesh_trust::admission::{admit, sign_admission_transcript};

    /// THE BLOCKER test (design §5.1/§8): the prover builds a transcript +
    /// signs; the broker reconstructs the transcript from the wire response;
    /// the two `canonical_bytes()` MUST be byte-identical and `admit()` MUST
    /// accept. This pins the populate-from-wire codec boundary on both sides.
    #[tokio::test]
    async fn round_trip_transcript_is_byte_identical() {
        use ed25519_dalek::SigningKey;
        use rand::rngs::OsRng;

        // A provisioned d2 keypair (the seed + the pubkey in the member record).
        let signing = SigningKey::generate(&mut OsRng);
        let seed = signing.to_bytes();
        let pubkey_b64 = B64.encode(signing.verifying_key().to_bytes());
        let member = serde_json::json!({
            "name": "delta",
            "bus": true,
            "status": "active",
            "credentials": [
                { "kind": "d2", "pubkey": pubkey_b64, "from_epoch": 1, "until_epoch": null }
            ]
        });

        // Broker issues a challenge.
        let table = ChallengeTable::new();
        let issued = table.issue(7, "bus", "beta").await.expect("issue");
        let challenge_body: Value = serde_json::from_str(&issued.body).unwrap();

        // PROVER side: build the transcript from the challenge body (decoded raw
        // bytes), sign.
        let prover_transcript = AdmissionTranscript {
            mesh_fqdn: challenge_body["mesh_fqdn"].as_str().unwrap().to_string(),
            claimed_source_node: "delta".to_string(),
            verifying_broker_node: challenge_body["verifying_broker_node"]
                .as_str()
                .unwrap()
                .to_string(),
            inventory_epoch: challenge_body["inventory_epoch"].as_u64().unwrap(),
            session_id: B64
                .decode(challenge_body["session_id"].as_str().unwrap())
                .unwrap(),
            server_nonce: B64
                .decode(challenge_body["server_nonce"].as_str().unwrap())
                .unwrap(),
            client_ephemeral: vec![0u8; 32],
            channel_binding_hash: vec![0u8; 32],
        };
        let sig = sign_admission_transcript(&seed, &prover_transcript).unwrap();
        let response_body = serde_json::json!({
            "claimed_source_node": "delta",
            "signed_epoch": 7,
            "signature": B64.encode(sig),
            "client_ephemeral": B64.encode([0u8; 32]),
            "channel_binding_hash": B64.encode([0u8; 32]),
        });

        // BROKER side: take the stored challenge, reconstruct, admit.
        let stored = table.take(&issued.id).await.expect("take");
        let proof = reconstruct_proof(&stored, &response_body, "bus", "beta").expect("reconstruct");

        // Byte-for-byte equality — the whole point.
        assert_eq!(
            proof.transcript.canonical_bytes(),
            prover_transcript.canonical_bytes(),
            "broker-reconstructed transcript must be byte-identical to the prover's"
        );
        assert_eq!(
            admit(&member, &proof.transcript, &proof.signature, 7),
            Ok(()),
            "a faithfully reconstructed, validly-signed proof must admit"
        );
        // And the digest matches across both sides (the divergence discriminator).
        assert_eq!(
            canon_digest(&proof.transcript),
            canon_digest(&prover_transcript)
        );
    }

    #[tokio::test]
    async fn challenge_table_single_use_and_reap() {
        let table = ChallengeTable::new();
        let issued = table.issue(1, "bus", "beta").await.unwrap();
        // single-use: first take succeeds, second finds nothing.
        assert!(table.take(&issued.id).await.is_some());
        assert!(table.take(&issued.id).await.is_none());
        // reap of an already-taken id is a no-op (no panic).
        table.reap(&issued.id).await;
        // a fresh challenge reaped before any take is gone.
        let i2 = table.issue(1, "bus", "beta").await.unwrap();
        table.reap(&i2.id).await;
        assert!(table.take(&i2.id).await.is_none());
    }

    #[tokio::test]
    async fn take_rejects_expired_challenge() {
        // A response past the TTL must NOT admit, even if no later issue() reaped
        // it. Backdate the entry directly (avoids a real CHALLENGE_TTL wait).
        let table = ChallengeTable::new();
        let issued = table.issue(1, "bus", "beta").await.unwrap();
        {
            let mut map = table.map.write().await;
            map.get_mut(&issued.id).unwrap().issued_at =
                Instant::now() - CHALLENGE_TTL - Duration::from_secs(1);
        }
        assert!(table.take(&issued.id).await.is_none(), "expired → stale");
        assert!(
            table.take(&issued.id).await.is_none(),
            "and removed (single-use)"
        );
    }

    #[tokio::test]
    async fn reconstruct_rejects_nonzero_ephemeral_origin_only() {
        let table = ChallengeTable::new();
        let issued = table.issue(1, "bus", "beta").await.unwrap();
        let stored = table.take(&issued.id).await.unwrap();
        let bad = serde_json::json!({
            "claimed_source_node": "delta",
            "signature": B64.encode([0u8; 64]),
            "client_ephemeral": B64.encode([1u8; 32]), // non-zero → hardened, not origin-only
            "channel_binding_hash": B64.encode([0u8; 32]),
        });
        assert_eq!(
            reconstruct_proof(&stored, &bad, "bus", "beta").unwrap_err(),
            WireError::Malformed
        );
    }
}
