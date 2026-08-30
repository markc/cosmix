//! SPEC 13 §9a D2 broker admission — transcript + verify primitives.
//!
//! **This is Phase 2 slice 2-a (the foundational, gate-free half).** WG peers
//! are a superset of members (INV-2), so a broker MUST gate every D2
//! (websocket/Bus) session with a per-session **credential** challenge, not
//! source IP (B1, §9a). On session open the broker sends a fresh nonce; the
//! connecting node signs the canonical admission transcript with its
//! `kind:"d2"` Ed25519 key; the broker verifies that against the claimed
//! node's current `d2` credential in its accepted inventory and admits only
//! if the member is `active`, `bus:true`, the credential's epoch window
//! covers the current epoch, and (caller's overlay) the node is not
//! deny-listed.
//!
//! Like [`crate::inventory`] (1b-a), this module is **pure crypto + types,
//! mesh-free core** — no broker, no IO, no async. It shares
//! [`crate::sig::verify_ed25519`] so signer (node) and verifier (broker)
//! cannot disagree on what a signature covers.
//!
//! # The signed transcript (§9a, field set + order FIXED; wire-encoding "to
//! ratify")
//!
//! ```text
//! "cosmix-d2-admit-v1" ‖ mesh_fqdn ‖ claimed_source_node ‖
//!     verifying_broker_node ‖ inventory_epoch ‖ session_id ‖
//!     server_nonce ‖ client_ephemeral ‖ channel_binding_hash
//! ```
//!
//! - The leading **domain tag** separates this Ed25519 use from any other.
//! - `verifying_broker_node` + `session_id` defeat replay/relay by any party
//!   that is not the relaying hub; `channel_binding_hash` extends that to the
//!   relaying hub only in the hardened profile (bound to the E2E shared
//!   secret, §9a). `inventory_epoch` ties the proof to a known epoch.
//! - **Fields a peer cannot yet provide are fixed-length zero, never omitted**
//!   (§9a) — the caller supplies the canonical zero bytes (e.g. an all-zero
//!   `client_ephemeral` in the origin-only profile), so the transcript can't
//!   be made ambiguous by dropping a field.
//!
//! This crate's [`AdmissionTranscript::canonical_bytes`] realises the fixed
//! field set with an **injective length-prefixed encoding** (each field:
//! 4-byte big-endian length, then bytes) — the candidate wire-encoding the
//! §9a "to ratify" note leaves open. It is injective (no field-boundary
//! ambiguity) and deterministic, so two implementations agree.
//!
//! # Phase-2 slices that build on this
//!
//! - **2-a (this module):** the [`AdmissionTranscript`] + [`select_d2_pubkeys`]
//!   credential selection + the [`admit`] membership/crypto gate. Gate-free,
//!   unit-tested.
//! - **2-b (landed):** the client/prover side — [`sign_admission_transcript`]
//!   signs the transcript with a node's `kind:"d2"` private key (the exact
//!   mirror of [`admit`], via [`crate::sig::sign_ed25519`]). The signer
//!   (`cosmix-mesh-sign`) needs no change to carry the d2 **pubkey**: a
//!   member's `credentials[]` rides through signing as opaque `members` data
//!   (covered by the signed canonical bytes), proven end-to-end by the
//!   `inventory` tests. Key GENERATION/storage is deliberately deferred to 2-d
//!   (it needs the OS RNG + the filesystem; this crate stays pure).
//! - **2-c:** the broker integration in `noded` — nonce challenge on session
//!   open, verify via this module against the accepted inventory, refuse with
//!   §9 `admission-refused`. **Gated on §7.8 B3** (delivery-on-reconnect,
//!   §14): reload tears down + re-establishes D2 sessions, and B3 defines the
//!   in-flight-request fate across that — so 2-c must not ship until B3 is
//!   resolved (decide-by-doing).
//! - **2-d:** d2 credential provisioning (per-node keypair generation + storage
//!   at install, like the `kind:"wg"` key, §6.1).

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use ed25519_dalek::SIGNATURE_LENGTH;
use serde_json::Value;

use crate::sig::{SigError, sign_ed25519, verify_ed25519};

/// Domain-separation tag for the D2 admission signature (§9a). A bump here is
/// a `schema_version` change, never an in-payload choice.
pub const ADMIT_DOMAIN_V1: &str = "cosmix-d2-admit-v1";
/// The credential `kind` that authorises D2 admission (§6.1).
pub const D2_CRED_KIND: &str = "d2";
/// The credential `kind` that carries the D0 WireGuard transport key (§6.1) —
/// the X25519 public key `cosmix-wgd` derives its intended kernel peer set
/// from. Consumed by the WG control plane, never by D2 admission.
pub const WG_CRED_KIND: &str = "wg";

const ED25519_PUBKEY_LEN: usize = 32;
/// X25519 (WireGuard) public keys are 32 bytes, same width as Ed25519 — named
/// separately so the two credential paths never accidentally share a constant.
const X25519_PUBKEY_LEN: usize = 32;

/// The fields a D2 admission signature covers (§9a). Field order is fixed by
/// the spec; [`Self::canonical_bytes`] is injective so signer and verifier
/// agree byte-for-byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionTranscript {
    pub mesh_fqdn: String,
    /// The node the session claims to be (matched against a member-record).
    pub claimed_source_node: String,
    /// The broker doing the verifying — binds the proof to THIS verifier so a
    /// relaying hub can't replay it onto its own session.
    pub verifying_broker_node: String,
    /// The accepted inventory epoch — ties the proof to a known membership.
    pub inventory_epoch: u64,
    /// Broker-assigned, unique per session.
    pub session_id: Vec<u8>,
    /// Fresh, unpredictable (CSPRNG), single-use; broker rejects a reused one.
    pub server_nonce: Vec<u8>,
    /// E2E DH share in the hardened profile; fixed-length zero in origin-only.
    pub client_ephemeral: Vec<u8>,
    /// Transport channel binding (hardened: over the E2E secret); fixed-length
    /// zero where unavailable.
    pub channel_binding_hash: Vec<u8>,
}

impl AdmissionTranscript {
    /// The exact bytes the d2 signature is computed over. Injective
    /// length-prefixed encoding (4-byte BE length + bytes per field), with the
    /// domain tag first and `inventory_epoch` as 8-byte big-endian. See the
    /// module docs; this is the §9a candidate wire-encoding (field set/order
    /// fixed by spec, exact encoding "to ratify").
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(256);
        push_field(&mut out, ADMIT_DOMAIN_V1.as_bytes());
        push_field(&mut out, self.mesh_fqdn.as_bytes());
        push_field(&mut out, self.claimed_source_node.as_bytes());
        push_field(&mut out, self.verifying_broker_node.as_bytes());
        push_field(&mut out, &self.inventory_epoch.to_be_bytes());
        push_field(&mut out, &self.session_id);
        push_field(&mut out, &self.server_nonce);
        push_field(&mut out, &self.client_ephemeral);
        push_field(&mut out, &self.channel_binding_hash);
        out
    }
}

/// Append a length-prefixed field (4-byte BE length, then the bytes) — makes
/// the concatenation injective so no two distinct field tuples collide.
/// Panics if a field exceeds `u32::MAX` (impossible for valid session data —
/// names, 32-byte nonces/hashes, an 8-byte epoch — and silently truncating
/// would break the injectivity the length prefix exists for).
fn push_field(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = u32::try_from(bytes.len())
        .expect("admission transcript field exceeds u32::MAX (impossible for valid session data)");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
}

/// Why a D2 admission was refused — variants map onto the §9
/// `admission-refused` detail strings.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdmissionError {
    #[error("source member {0:?} is tombstoned / not active")]
    SourceTombstoned(String),

    #[error("source member {0:?} is not an bus member (bus:false)")]
    SourceBusFalse(String),

    #[error("transcript claims node {claimed:?} but the member-record is {record:?}")]
    NameMismatch { claimed: String, record: String },

    #[error("transcript inventory_epoch {transcript} != the broker's accepted epoch {broker}")]
    EpochMismatch { transcript: u64, broker: u64 },

    #[error("no current d2 credential for {0:?} at the active epoch")]
    NoCurrentD2Credential(String),

    #[error("d2 admission signature for {0:?} did not verify (bad credential signature)")]
    BadCredentialSignature(String),

    #[error("malformed member-record: {0}")]
    MalformedMember(&'static str),
}

/// Select **all** currently-valid `kind:"d2"` Ed25519 public keys for a member
/// at `current_epoch`. The window is `[from_epoch, until_epoch)` — **half-open**
/// (§6.1): `until_epoch` is the roll at which the outgoing key stops being
/// valid, so `current_epoch >= until_epoch` is out; `until_epoch: null`/absent
/// is open-ended. During a **rotation overlap** two `d2` keys' windows overlap
/// and BOTH must admit (§6.1/§9a), so every current key is returned — the
/// caller admits if any of them verifies. Fails closed: a `d2` credential
/// missing `from_epoch`, with a malformed `until_epoch`, or a non-32-byte
/// pubkey is **skipped** (never defaulted to "valid from genesis").
pub fn select_d2_pubkeys(member: &Value, current_epoch: u64) -> Vec<Vec<u8>> {
    let Some(creds) = member.get("credentials").and_then(Value::as_array) else {
        return Vec::new();
    };
    creds
        .iter()
        .filter_map(|c| {
            if c.get("kind").and_then(Value::as_str) != Some(D2_CRED_KIND) {
                return None;
            }
            // from_epoch is REQUIRED — a missing/malformed one fails closed.
            let from = c.get("from_epoch").and_then(Value::as_u64)?;
            if current_epoch < from {
                return None;
            }
            // until_epoch: u64 → half-open [from, until); null/absent → open.
            match c.get("until_epoch") {
                None | Some(Value::Null) => {}
                Some(v) => {
                    let until = v.as_u64()?; // malformed → fail closed (skip)
                    if current_epoch >= until {
                        return None;
                    }
                }
            }
            let pk = B64.decode(c.get("pubkey").and_then(Value::as_str)?).ok()?;
            (pk.len() == ED25519_PUBKEY_LEN).then_some(pk)
        })
        .collect()
}

/// Select **all** currently-valid `kind:"wg"` X25519 public keys for a member
/// at `current_epoch`, decoded to raw 32-byte form. The exact mirror of
/// [`select_d2_pubkeys`] — same half-open `[from_epoch, until_epoch)` window
/// (§6.1) and the same fail-closed skips (missing `from_epoch`, malformed
/// `until_epoch`, non-32-byte pubkey) — but selects the D0 transport credential
/// (§6.1) instead of the D2 admission one.
///
/// This is the single audited parse site for turning an opaque signed-inventory
/// member record into its WireGuard public identity, so `cosmix-wgd`'s
/// derive-from-inventory path (and any future consumer, e.g. dnsd synthesising
/// peer DNS) share one credential-selection semantics rather than each
/// re-walking the untyped `credentials[]` JSON. A member in a **rotation
/// overlap** yields both current keys; the reconciler treats every returned key
/// as an accepted transport identity for that member at this epoch.
///
/// Returns raw X25519 bytes (base64-decoded); the caller reconstructs a typed
/// `WgPublicKey` (e.g. `cosmix_wg::WgPublicKey::from_base64` after re-encoding,
/// or a `from_bytes` constructor). Bytes — not the base64 string — so the
/// selection semantics are byte-identical to the d2 path and a downstream
/// dedup/diff keys on canonical 32-byte values, not on base64 spelling.
pub fn select_wg_pubkeys(member: &Value, current_epoch: u64) -> Vec<Vec<u8>> {
    let Some(creds) = member.get("credentials").and_then(Value::as_array) else {
        return Vec::new();
    };
    creds
        .iter()
        .filter_map(|c| {
            if c.get("kind").and_then(Value::as_str) != Some(WG_CRED_KIND) {
                return None;
            }
            // from_epoch is REQUIRED — a missing/malformed one fails closed.
            let from = c.get("from_epoch").and_then(Value::as_u64)?;
            if current_epoch < from {
                return None;
            }
            // until_epoch: u64 → half-open [from, until); null/absent → open.
            match c.get("until_epoch") {
                None | Some(Value::Null) => {}
                Some(v) => {
                    let until = v.as_u64()?; // malformed → fail closed (skip)
                    if current_epoch >= until {
                        return None;
                    }
                }
            }
            let pk = B64.decode(c.get("pubkey").and_then(Value::as_str)?).ok()?;
            (pk.len() == X25519_PUBKEY_LEN).then_some(pk)
        })
        .collect()
}

/// The d2 **prover** (§9a slice 2-b): a connecting node signs the admission
/// transcript with its `kind:"d2"` Ed25519 private key (32-byte seed). The exact
/// mirror of [`admit`]'s verify step — both sign/verify
/// [`AdmissionTranscript::canonical_bytes`], so a node's proof and the broker's
/// check cannot disagree on what the signature covers. The node builds the
/// transcript from the broker's challenge (`session_id`, `server_nonce`,
/// `verifying_broker_node`) plus its own claimed identity and the accepted
/// `inventory_epoch`, signs it here, and returns the 64-byte signature for the
/// broker to feed to [`admit`].
pub fn sign_admission_transcript(
    d2_private_key: &[u8],
    transcript: &AdmissionTranscript,
) -> Result<[u8; SIGNATURE_LENGTH], SigError> {
    sign_ed25519(d2_private_key, &transcript.canonical_bytes())
}

/// The full §9a admission gate for one session: the member must be `active`
/// and `bus:true`, the transcript's `claimed_source_node` must be this
/// member, a current `d2` credential must exist, and the `signature` must
/// verify over the transcript against that credential's pubkey.
///
/// The **deny-list** check (§7.5) is the caller's overlay — apply it before
/// this — and so is **nonce freshness / single-use** (broker session state).
/// This function is the stateless cryptographic + membership core.
pub fn admit(
    member: &Value,
    transcript: &AdmissionTranscript,
    signature: &[u8],
    current_epoch: u64,
) -> Result<(), AdmissionError> {
    let name = member
        .get("name")
        .and_then(Value::as_str)
        .ok_or(AdmissionError::MalformedMember("member has no name"))?;

    // Bind the proof to the broker's accepted epoch: the (signed) transcript
    // MUST claim the same epoch the broker selects the credential at, else the
    // "ties the proof to a known epoch" property (§9a) is not enforced.
    if transcript.inventory_epoch != current_epoch {
        return Err(AdmissionError::EpochMismatch {
            transcript: transcript.inventory_epoch,
            broker: current_epoch,
        });
    }
    if member.get("status").and_then(Value::as_str) != Some("active") {
        return Err(AdmissionError::SourceTombstoned(name.to_string()));
    }
    if member.get("bus").and_then(Value::as_bool) != Some(true) {
        return Err(AdmissionError::SourceBusFalse(name.to_string()));
    }
    if name != transcript.claimed_source_node {
        return Err(AdmissionError::NameMismatch {
            claimed: transcript.claimed_source_node.clone(),
            record: name.to_string(),
        });
    }

    let d2_keys = select_d2_pubkeys(member, current_epoch);
    if d2_keys.is_empty() {
        return Err(AdmissionError::NoCurrentD2Credential(name.to_string()));
    }

    // Admit if ANY current d2 key verifies — during a rotation overlap both
    // the outgoing and incoming keys are valid (§6.1).
    let msg = transcript.canonical_bytes();
    if d2_keys
        .iter()
        .any(|pk| verify_ed25519(pk, signature, &msg).is_ok())
    {
        Ok(())
    } else {
        Err(AdmissionError::BadCredentialSignature(name.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use rand::rngs::OsRng;
    use serde_json::json;

    fn keypair() -> (SigningKey, String) {
        let sk = SigningKey::generate(&mut OsRng);
        (sk.clone(), B64.encode(sk.verifying_key().to_bytes()))
    }

    fn transcript(node: &str) -> AdmissionTranscript {
        AdmissionTranscript {
            mesh_fqdn: "bus".into(),
            claimed_source_node: node.into(),
            verifying_broker_node: "beta".into(),
            inventory_epoch: 7,
            session_id: vec![1, 2, 3, 4],
            server_nonce: vec![9u8; 32],
            client_ephemeral: vec![0u8; 32], // origin-only profile
            channel_binding_hash: vec![0u8; 32],
        }
    }

    fn member(name: &str, d2_pub: &str, status: &str, bus: bool, from: u64, until: Value) -> Value {
        json!({
            "name": name,
            "status": status,
            "bus": bus,
            "credentials": [
                { "kind": "wg", "pubkey": "AAAA", "from_epoch": 1, "until_epoch": null },
                { "kind": "d2", "pubkey": d2_pub, "from_epoch": from, "until_epoch": until },
            ],
        })
    }

    #[test]
    fn admits_a_valid_session() {
        let (sk, d2_pub) = keypair();
        let t = transcript("delta");
        let sig = sk.sign(&t.canonical_bytes());
        let m = member("delta", &d2_pub, "active", true, 1, Value::Null);
        assert_eq!(admit(&m, &t, sig.to_bytes().as_ref(), 7), Ok(()));
    }

    // ---- select_wg_pubkeys: the D0 transport-credential mirror of the d2
    // selector. Same window + fail-closed semantics, kind:"wg" instead of "d2".
    fn wg_member(creds: Value) -> Value {
        json!({ "name": "delta", "status": "active", "bus": true, "credentials": creds })
    }

    #[test]
    fn select_wg_returns_in_window_32_byte_keys_and_skips_d2() {
        let wg = B64.encode([7u8; X25519_PUBKEY_LEN]);
        let d2 = B64.encode([9u8; ED25519_PUBKEY_LEN]);
        let m = wg_member(json!([
            { "kind": "wg", "pubkey": wg, "from_epoch": 5, "until_epoch": null },
            { "kind": "d2", "pubkey": d2, "from_epoch": 1, "until_epoch": null },
        ]));
        // In window (epoch >= from, no until): exactly the one wg key, decoded.
        let got = select_wg_pubkeys(&m, 5);
        assert_eq!(
            got,
            vec![vec![7u8; X25519_PUBKEY_LEN]],
            "the d2 cred must not leak into the wg selection"
        );
    }

    #[test]
    fn select_wg_respects_half_open_window_and_fails_closed() {
        let wg = B64.encode([7u8; X25519_PUBKEY_LEN]);
        // Half-open [3, 6): epoch 2 too early, 6 is the roll (out), 5 in.
        let windowed = wg_member(json!([
            { "kind": "wg", "pubkey": wg, "from_epoch": 3, "until_epoch": 6 },
        ]));
        assert!(
            select_wg_pubkeys(&windowed, 2).is_empty(),
            "before from_epoch"
        );
        assert_eq!(select_wg_pubkeys(&windowed, 5).len(), 1, "inside window");
        assert!(
            select_wg_pubkeys(&windowed, 6).is_empty(),
            "until_epoch is exclusive"
        );

        // Fail-closed skips: missing from_epoch, malformed until_epoch, and a
        // non-32-byte pubkey (the "AAAA" = 3-byte fixture shape) are all dropped.
        let bad = wg_member(json!([
            { "kind": "wg", "pubkey": wg, "until_epoch": null },                 // no from_epoch
            { "kind": "wg", "pubkey": wg, "from_epoch": 1, "until_epoch": "x" }, // malformed until
            { "kind": "wg", "pubkey": "AAAA", "from_epoch": 1, "until_epoch": null }, // 3-byte key
        ]));
        assert!(
            select_wg_pubkeys(&bad, 5).is_empty(),
            "every malformed wg cred fails closed"
        );
    }

    #[test]
    fn select_wg_returns_both_keys_during_rotation_overlap() {
        let a = B64.encode([1u8; X25519_PUBKEY_LEN]);
        let b = B64.encode([2u8; X25519_PUBKEY_LEN]);
        // Outgoing [1,10), incoming [8, open): epoch 9 is inside both.
        let m = wg_member(json!([
            { "kind": "wg", "pubkey": a, "from_epoch": 1, "until_epoch": 10 },
            { "kind": "wg", "pubkey": b, "from_epoch": 8, "until_epoch": null },
        ]));
        assert_eq!(
            select_wg_pubkeys(&m, 9).len(),
            2,
            "rotation overlap yields both current transport keys"
        );
    }

    #[test]
    fn rejects_wrong_signing_key() {
        let (_real, d2_pub) = keypair();
        let (attacker, _) = keypair();
        let t = transcript("delta");
        let sig = attacker.sign(&t.canonical_bytes());
        let m = member("delta", &d2_pub, "active", true, 1, Value::Null);
        assert!(matches!(
            admit(&m, &t, sig.to_bytes().as_ref(), 7),
            Err(AdmissionError::BadCredentialSignature(_))
        ));
    }

    #[test]
    fn rejects_tampered_transcript() {
        // Sign one transcript, present another (different session_id) — a
        // relay onto a different session is refused.
        let (sk, d2_pub) = keypair();
        let signed = transcript("delta");
        let sig = sk.sign(&signed.canonical_bytes());
        let mut other = transcript("delta");
        other.session_id = vec![9, 9, 9]; // different session
        let m = member("delta", &d2_pub, "active", true, 1, Value::Null);
        assert!(matches!(
            admit(&m, &other, sig.to_bytes().as_ref(), 7),
            Err(AdmissionError::BadCredentialSignature(_))
        ));
    }

    #[test]
    fn rejects_tombstoned_and_non_bus() {
        let (sk, d2_pub) = keypair();
        let t = transcript("delta");
        let sig = sk.sign(&t.canonical_bytes());
        let tomb = member("delta", &d2_pub, "tombstoned", true, 1, Value::Null);
        assert!(matches!(
            admit(&tomb, &t, sig.to_bytes().as_ref(), 7),
            Err(AdmissionError::SourceTombstoned(_))
        ));
        let noamp = member("delta", &d2_pub, "active", false, 1, Value::Null);
        assert!(matches!(
            admit(&noamp, &t, sig.to_bytes().as_ref(), 7),
            Err(AdmissionError::SourceBusFalse(_))
        ));
    }

    #[test]
    fn rejects_name_mismatch() {
        let (sk, d2_pub) = keypair();
        let t = transcript("delta");
        let sig = sk.sign(&t.canonical_bytes());
        // The member-record is gamma, but the transcript claims delta.
        let m = member("gamma", &d2_pub, "active", true, 1, Value::Null);
        assert!(matches!(
            admit(&m, &t, sig.to_bytes().as_ref(), 7),
            Err(AdmissionError::NameMismatch { .. })
        ));
    }

    #[test]
    fn rejects_out_of_window_credential() {
        let (sk, d2_pub) = keypair();
        let t = transcript("delta"); // epoch 7
        let sig = sk.sign(&t.canonical_bytes());
        // d2 window [10, 20] does not cover epoch 7.
        let future = member("delta", &d2_pub, "active", true, 10, json!(20));
        assert!(matches!(
            admit(&future, &t, sig.to_bytes().as_ref(), 7),
            Err(AdmissionError::NoCurrentD2Credential(_))
        ));
        // d2 window [1, 5] expired before epoch 7.
        let expired = member("delta", &d2_pub, "active", true, 1, json!(5));
        assert!(matches!(
            admit(&expired, &t, sig.to_bytes().as_ref(), 7),
            Err(AdmissionError::NoCurrentD2Credential(_))
        ));
    }

    #[test]
    fn select_d2_skips_wg_and_picks_in_window() {
        let (_sk, d2_pub) = keypair();
        let m = member("delta", &d2_pub, "active", true, 1, Value::Null);
        // wg credential present first, but only the d2 one is selected.
        assert_eq!(select_d2_pubkeys(&m, 7), vec![B64.decode(&d2_pub).unwrap()]);
    }

    #[test]
    fn admits_with_either_overlapping_d2_key() {
        // Rotation overlap (§6.1): outgoing [1,8), incoming [5, null). At epoch
        // 7 both are valid; a session signed with EITHER must admit.
        let (out_sk, out_pub) = keypair();
        let (in_sk, in_pub) = keypair();
        let m = json!({
            "name": "delta", "status": "active", "bus": true,
            "credentials": [
                { "kind": "d2", "pubkey": out_pub, "from_epoch": 1, "until_epoch": 8 },
                { "kind": "d2", "pubkey": in_pub, "from_epoch": 5, "until_epoch": null },
            ],
        });
        assert_eq!(select_d2_pubkeys(&m, 7).len(), 2);
        let t = transcript("delta"); // epoch 7
        let sig_out = out_sk.sign(&t.canonical_bytes());
        assert_eq!(admit(&m, &t, sig_out.to_bytes().as_ref(), 7), Ok(()));
        let sig_in = in_sk.sign(&t.canonical_bytes());
        assert_eq!(admit(&m, &t, sig_in.to_bytes().as_ref(), 7), Ok(()));
    }

    #[test]
    fn until_epoch_is_half_open() {
        // [1, 5): valid at 4, NOT at 5.
        let (sk, d2_pub) = keypair();
        let m = member("delta", &d2_pub, "active", true, 1, json!(5));
        assert_eq!(select_d2_pubkeys(&m, 4).len(), 1);
        assert_eq!(select_d2_pubkeys(&m, 5).len(), 0);

        let mut t4 = transcript("delta");
        t4.inventory_epoch = 4;
        let sig4 = sk.sign(&t4.canonical_bytes());
        assert_eq!(admit(&m, &t4, sig4.to_bytes().as_ref(), 4), Ok(()));

        let mut t5 = transcript("delta");
        t5.inventory_epoch = 5;
        let sig5 = sk.sign(&t5.canonical_bytes());
        assert!(matches!(
            admit(&m, &t5, sig5.to_bytes().as_ref(), 5),
            Err(AdmissionError::NoCurrentD2Credential(_))
        ));
    }

    #[test]
    fn fails_closed_on_d2_missing_from_epoch() {
        let (_sk, d2_pub) = keypair();
        let m = json!({
            "name": "delta", "status": "active", "bus": true,
            "credentials": [ { "kind": "d2", "pubkey": d2_pub } ], // no from_epoch
        });
        assert_eq!(select_d2_pubkeys(&m, 7).len(), 0);
    }

    #[test]
    fn rejects_epoch_mismatch() {
        // The transcript claims epoch 7; the broker is verifying at epoch 8.
        let (sk, d2_pub) = keypair();
        let m = member("delta", &d2_pub, "active", true, 1, Value::Null);
        let t = transcript("delta"); // inventory_epoch 7
        let sig = sk.sign(&t.canonical_bytes());
        assert!(matches!(
            admit(&m, &t, sig.to_bytes().as_ref(), 8),
            Err(AdmissionError::EpochMismatch {
                transcript: 7,
                broker: 8
            })
        ));
    }

    #[test]
    fn prover_round_trips_through_the_gate() {
        // 2-b prover + 2-a gate end-to-end: the node signs with its d2 PRIVATE
        // key via sign_admission_transcript; the broker admits via admit(). No
        // manual sk.sign — proves the two helpers cover identical bytes.
        let sk = SigningKey::generate(&mut OsRng);
        let d2_pub = B64.encode(sk.verifying_key().to_bytes());
        let seed = sk.to_bytes(); // 32-byte d2 private key the node holds
        let t = transcript("delta");
        let sig = sign_admission_transcript(&seed, &t).unwrap();
        let m = member("delta", &d2_pub, "active", true, 1, Value::Null);
        assert_eq!(admit(&m, &t, &sig, 7), Ok(()));
    }

    #[test]
    fn prover_signature_is_session_bound() {
        // A signature the prover made for one session must NOT admit another
        // (different session_id) — the transcript binds the proof to a session.
        let sk = SigningKey::generate(&mut OsRng);
        let d2_pub = B64.encode(sk.verifying_key().to_bytes());
        let seed = sk.to_bytes();
        let signed = transcript("delta");
        let sig = sign_admission_transcript(&seed, &signed).unwrap();
        let mut replayed = transcript("delta");
        replayed.session_id = vec![7, 7, 7];
        let m = member("delta", &d2_pub, "active", true, 1, Value::Null);
        assert!(matches!(
            admit(&m, &replayed, &sig, 7),
            Err(AdmissionError::BadCredentialSignature(_))
        ));
    }

    #[test]
    fn prover_rejects_bad_private_key_length() {
        let t = transcript("delta");
        assert!(sign_admission_transcript(&[0u8; 16], &t).is_err());
    }

    #[test]
    fn canonical_bytes_are_injective_across_fields() {
        // Moving a byte from one field to the adjacent one changes the
        // transcript (length-prefixing prevents boundary ambiguity).
        let mut a = transcript("delta");
        a.claimed_source_node = "ab".into();
        a.verifying_broker_node = "c".into();
        let mut b = transcript("delta");
        b.claimed_source_node = "a".into();
        b.verifying_broker_node = "bc".into();
        assert_ne!(a.canonical_bytes(), b.canonical_bytes());
    }
}
