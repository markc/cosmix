//! SPEC 13 §7.1/§7.2 signed mesh inventory — types + verification.
//!
//! The mesh inventory is the **trust root** (INV-1): the single
//! authored document declaring mesh membership + identity. Phase 1a
//! authored it *unsigned* (a generator input); Phase 1b wraps it in a
//! detached-signature envelope so a node can boot and route from its
//! locally-cached *verified* authority, never a runtime dependency on
//! the hub (INV-7).
//!
//! This module is the **verifier half** (SPEC 13 1b-a). It carries no
//! mesh-private data and no IO/async — pure crypto + types — so it
//! lives in the crate *core* (`--no-default-features`). The Rust signer
//! (1b-b) and `noded`'s load/enforce path (1b-c) build on it; both
//! signer and verifier reach the same signature bytes by both calling
//! the one shared [`crate::canonical`] canonicaliser (the cardinal
//! anti-divergence mitigation, SPEC 13 §6).
//!
//! # Envelope shape (§7.1)
//!
//! ```text
//! signed-inventory = {
//!   payload    = { schema_version, canonical_encoding, mesh, subnet,
//!                  epoch, signed_at, valid_until, hub[], verify_keys[],
//!                  members[], (recovery, recovery_generation) },
//!   signatures = [ { key_id, alg, sig } ]   -- ≥1, DISTINCT key_ids
//! }
//! ```
//!
//! The signature is **outside** the bytes it signs — a classic foot-gun
//! is signing a struct that contains its own signature. Each signature
//! is over `canonical_encoding(payload)`.
//!
//! # What [`SignedInventory::verify`] enforces
//!
//! - **`canonical_encoding` validated against a closed set FIRST**
//!   (§7.2) — before canonicalising — so the self-described field cannot
//!   pick an attacker-favourable encoding.
//! - **Algorithm pinned to Ed25519** (§7.2): only an `Ed25519` signature
//!   `alg` can count toward acceptance (no `alg:none`, no downgrade); a
//!   `verify_keys` `key_type` other than `Ed25519` is rejected.
//! - **At least one signature** by a `key_id` that is both **declared in
//!   `payload.verify_keys`** (§7.1) and **already trusted** by the node
//!   (active|retiring) must verify over the re-canonicalised bytes —
//!   verified against the *trusted* public key, never the payload-claimed
//!   one (§7.2). A signature naming a key the node does not yet trust is
//!   skipped, which is exactly what lets a node trusting only the old key
//!   accept a dual-signed rotation inventory and learn the new verify key
//!   in-band (§6.4).
//! - **Signatures are attacker-malleable.** They sit *outside* the signed
//!   bytes, so a hostile carrier can append, duplicate, or corrupt
//!   `signatures[]` entries without breaking the payload signature. Every
//!   invalid/unrecognised entry is therefore **skipped, never a hard
//!   reject** (hard-rejecting on one appended junk entry would be a
//!   trivial denial-of-update); each `key_id` is counted at most once (no
//!   dup-sig fake quorum). `verify_keys[]` (which IS signed) must instead
//!   have distinct `key_id`s.
//! - **Epoch monotonicity** (§7.2): a normal inventory whose `epoch` is
//!   *older* than the node's cached epoch is rejected even if the
//!   signature verifies (rollback protection).
//! - **Normal generation carriage** (§6.4): a generation-silent normal
//!   inventory is accepted as generation zero only while the cached recovery
//!   generation is zero (the migration window). Once the cached generation is
//!   above zero it is rejected as [`VerifyError::NormalMissingRecoveryGeneration`]
//!   regardless of its epoch. The `recovery` key is presence-marked: normal
//!   payloads omit it, and an explicit `recovery:false` is rejected.
//! - **In-band verify-key adoption** (§7.2/§6.4): on accept the caller
//!   adopts `payload.verify_keys` as its new trusted set ([`AcceptedInventory::adopt_verify_keys`]).
//! - **Genesis anti-lockout rule 1** (§7.2): the adopted `verify_keys`
//!   set MUST still contain the genesis key — unconditionally.
//! - **Recovery override** (§6.4): a `recovery: true` payload signed by
//!   the **genesis** key, carrying a `recovery_generation` greater than
//!   the node's last, supersedes the epoch axis — the one sanctioned bypass
//!   of rollback protection. An older generation cannot replay; an equal
//!   generation is accepted only when its canonical hash exactly matches the
//!   pinned payload. Legacy hashless migration is normal-path only.
//! - **The recovery floor only rises under genesis.** A normal inventory
//!   may *echo* the current `recovery_generation`, but it may only
//!   *raise* the floor when genesis-signed (and never roll it back) — so
//!   a stolen *active* key cannot pin the floor at `u64::MAX` and
//!   permanently lock out genesis recovery (§6.4).
//!
//! # Deliberate non-goals of this slice
//!
//! - **No wall-clock gating.** The mesh has no trusted time source
//!   (§16a); `signed_at` / `valid_until` are advisory only and are NOT
//!   consulted here. The security trigger is epoch/generation-driven.
//! - **Anti-lockout rule 2** (a `retiring` key must not be the *sole*
//!   authoriser of a payload that adds/promotes a `verify_keys` entry,
//!   §7.2) is **not** enforced in 1b-a — the testbed runs a single
//!   signing key (B2) with no `retiring` key, and the promote-detection
//!   logic lands with the multi-key rotation slice. Tracked, not
//!   forgotten.
//! - **No member-record interpretation.** Admission/routing off the
//!   member set is Phase 2 / 1b-c; here `members` is carried as opaque
//!   (but strictly-parsed, dup-key-rejecting) signed data.
//! - **No persistence.** The `(epoch, recovery_generation)` baseline is
//!   an *input* ([`NodeTrustState`]); durable storage is 1b-c.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;

use crate::canonical::to_canonical_bytes;
use crate::sig::verify_ed25519;

/// The only inventory schema version this verifier understands. A
/// higher `schema_version` gates §16 negotiation and is rejected
/// (fail-closed forward-compat): a v1 verifier must not guess at v2
/// semantics.
pub const SUPPORTED_SCHEMA_VERSION: u32 = 1;

/// The sole permitted `canonical_encoding` value (§7.2 closed enum):
/// RFC 8785 JCS, realised here by the shared [`crate::canonical`] v1
/// emitter (JCS-equivalent over the inventory value space).
pub const CANONICAL_ENCODING_V1: &str = "json/rfc8785";

/// The sole permitted signature algorithm (§7.2) and `verify_keys`
/// key type. Adding an algorithm is a `schema_version` bump, never an
/// in-payload free choice.
pub const ALG_ED25519: &str = "Ed25519";

/// Fields supplied by the signing ceremony rather than the authored
/// inventory. This is the single shared definition used both to reject
/// signer-owned fields in an authored input and to derive its stable semantic
/// identity with [`authoring_blake3_for_value`].
pub const SIGNER_OWNED_FIELDS: &[&str] = &[
    "canonical_encoding",
    "signed_at",
    "valid_until",
    "verify_keys",
    "signatures",
    "recovery",
    "recovery_generation",
];

const ED25519_PUBKEY_LEN: usize = 32;
const ED25519_SIG_LEN: usize = 64;

/// Lifecycle status of a verify key (§7.1). A verify key's lifetime is
/// set-membership-driven (valid while present as active|retiring,
/// retired by removal), *not* epoch-windowed like node credentials.
/// Unknown status strings (e.g. `tombstoned`) fail to deserialize —
/// fail-closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyStatus {
    Active,
    Retiring,
}

/// A trust-anchor entry in `payload.verify_keys` (§7.1). `pubkey` is
/// base64 on the wire and is preserved verbatim for canonicalisation;
/// it is decoded only to validate it (so a malformed key is never
/// adopted) and, after acceptance, by the caller installing the new
/// trusted set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifyKey {
    pub key_id: String,
    /// Base64-encoded Ed25519 public key (32 bytes).
    pub pubkey: String,
    /// Closed enum; `Ed25519` today (§7.2).
    pub key_type: String,
    pub status: KeyStatus,
}

/// One detached signature over `canonical_encoding(payload)` (§7.1).
///
/// This sits in the UNSIGNED, carrier-malleable signature bag (see
/// [`SignedInventory`]), so it deliberately does **not** use
/// `deny_unknown_fields`: a hostile carrier could otherwise append an
/// entry with a junk field and DoS-fail the whole parse before the
/// skip-the-invalid logic in [`SignedInventory::verify`] ever runs.
/// Unknown fields are ignored; the entry is judged on its `key_id` /
/// `alg` / `sig` alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvSignature {
    /// Names a `verify_keys` entry; the verifier checks it against the
    /// key the node ALREADY trusts under this `key_id`.
    pub key_id: String,
    /// Pinned to `Ed25519` (§7.2); any other value is rejected.
    pub alg: String,
    /// Base64-encoded Ed25519 signature (64 bytes).
    pub sig: String,
}

/// The signed payload (§7.1) — the only hand-authored content, and the
/// exact set of fields the signature covers.
///
/// Field order in this struct does not affect canonical bytes:
/// [`canonical_bytes`](Self::canonical_bytes) sorts keys
/// lexicographically. `recovery` / `recovery_generation` are omitted
/// from the canonical bytes when absent. Generation-zero legacy normal
/// inventories may omit both; current normal inventories carry
/// `recovery_generation` without `recovery`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryPayload {
    pub schema_version: u32,
    pub canonical_encoding: String,
    pub mesh: String,
    pub subnet: String,
    pub epoch: u64,
    /// Advisory wall-clock only (§16a) — never a security trigger.
    pub signed_at: String,
    /// Advisory wall-clock hint only (§7.5/§16) — never a security
    /// trigger.
    pub valid_until: String,
    pub hub: Vec<String>,
    pub verify_keys: Vec<VerifyKey>,
    /// `[node-record...]` (§6.1). Carried as opaque-but-strict data in
    /// 1b-a (members are signed, not yet interpreted). Strict parsing
    /// rejects duplicate object keys recursively.
    #[serde(deserialize_with = "crate::canonical::deserialize_strict_value")]
    pub members: Value,
    /// Recovery marker (§6.4). Present + `true` only on a genesis-signed
    /// recovery inventory. Omitted from canonical bytes when `None`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub recovery: Option<bool>,
    /// Monotonic recovery counter (§6.4). Required when `recovery` is
    /// `true`. On a normal inventory it carries the current baseline
    /// (cold-boot inheritance). Omitted from canonical bytes when
    /// `None`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub recovery_generation: Option<u64>,
}

impl InventoryPayload {
    /// Produce the v1 canonical bytes — the exact byte sequence the
    /// signature is computed over (§7.2). Goes through the one shared
    /// [`crate::canonical`] canonicaliser, so signer and verifier agree
    /// by construction.
    ///
    /// The struct's `Serialize` impl (with `skip_serializing_if` on the
    /// recovery fields) yields the JSON object; the canonicaliser then
    /// sorts keys recursively and emits minimal-escaped, whitespace-free
    /// bytes.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        // serde_json::to_value of a plain data struct cannot fail (no
        // map with non-string keys, no custom Serialize that errors).
        let v =
            serde_json::to_value(self).expect("InventoryPayload always serialises to a JSON value");
        to_canonical_bytes(&v)
    }

    /// Lowercase BLAKE3 hex of the exact canonical payload bytes signed and
    /// reported by the node authority plane.
    pub fn canonical_blake3(&self) -> String {
        blake3::hash(&self.canonical_bytes()).to_hex().to_string()
    }

    /// Lowercase BLAKE3 hex of the authored semantic projection. Signing-time
    /// metadata and the unsigned authoring marker are removed by the one
    /// shared [`authoring_blake3_for_value`] normaliser, so re-signing unchanged
    /// authored content does not change this identity.
    pub fn authoring_blake3(&self) -> String {
        let value =
            serde_json::to_value(self).expect("InventoryPayload always serialises to a JSON value");
        authoring_blake3_for_value(&value)
    }
}

/// Lowercase BLAKE3 hex of an inventory value after projecting it onto the
/// authored semantic surface: remove [`SIGNER_OWNED_FIELDS`] and the Phase-1a
/// `unsigned` marker, then hash the shared canonical encoding.
///
/// Both a verified [`InventoryPayload`] and the signer's strict-data JSON
/// conversion call this function, keeping `--against-authored` and provenance
/// stamping on one normalisation contract.
pub fn authoring_blake3_for_value(value: &Value) -> String {
    let mut projected = value.clone();
    if let Some(object) = projected.as_object_mut() {
        object.remove("unsigned");
        for field in SIGNER_OWNED_FIELDS {
            object.remove(*field);
        }
    }
    blake3::hash(&to_canonical_bytes(&projected))
        .to_hex()
        .to_string()
}

/// The signed-inventory envelope (§7.1) — the JSON wire artifact
/// (`/var/lib/cosmix/noded/inventory.signed`).
///
/// The **strict / tolerant boundary** is deliberate:
/// - [`InventoryPayload`] and its nested [`VerifyKey`] are the SIGNED
///   content and use `deny_unknown_fields` + strict (dup-key-rejecting)
///   parsing — an unknown or smuggled field there must hard-fail, so a
///   v2 field cannot be downgraded past a v1 verifier and nothing can
///   slip into the canonical bytes unnoticed.
/// - This envelope wrapper and the [`InvSignature`] bag are UNSIGNED and
///   carrier-malleable, so they parse tolerantly (no `deny_unknown_fields`).
///   An appended top-level field cannot affect the signed payload's
///   verification, and rejecting it would only hand a relaying carrier a
///   cheap denial-of-update. `signatures` is parsed leniently (see
///   [`deserialize_lenient_signatures`]): a malformed *entry* is dropped,
///   not a parse failure — completing the same carrier-malleability
///   posture that [`SignedInventory::verify`] applies semantically.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedInventory {
    pub payload: InventoryPayload,
    #[serde(deserialize_with = "deserialize_lenient_signatures")]
    pub signatures: Vec<InvSignature>,
}

/// Lenient deserializer for the carrier-malleable signature bag: parse
/// each array element as a raw JSON value and keep only those that form a
/// well-shaped [`InvSignature`], silently dropping the rest. A hostile
/// carrier can append malformed entries (`{}`, a missing field, a
/// wrong-typed `sig`) to an otherwise valid artifact; dropping them here
/// means one junk entry cannot DoS-fail the whole parse. Semantic validity
/// (alg pinning, trust, declaration, signature bytes) is still judged in
/// [`SignedInventory::verify`]. The outer value must still be a JSON array;
/// a wholly non-array `signatures` (or its absence) is a corruption
/// equivalent to dropping the artifact, which a carrier can always do.
fn deserialize_lenient_signatures<'de, D>(d: D) -> Result<Vec<InvSignature>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Vec::<Value>::deserialize(d)?;
    Ok(raw
        .into_iter()
        .filter_map(|v| serde_json::from_value::<InvSignature>(v).ok())
        .collect())
}

/// A verify key the node **already trusts** (its current anchor set),
/// with the public key already decoded to bytes. Distinct from
/// [`VerifyKey`] (the wire/payload form): verification always uses the
/// trusted key under a `key_id`, never the key the payload claims.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedKey {
    pub key_id: String,
    /// Raw 32-byte Ed25519 public key.
    pub pubkey: Vec<u8>,
    pub status: KeyStatus,
}

/// The node's complete trust state at verification time: the set of
/// verify keys it currently trusts, which of them is genesis (the
/// non-removable anchor), and the persisted anti-rollback baseline.
///
/// In 1b-a this is an *input*; 1b-c wires it to durable storage
/// (`/var/lib/cosmix/noded/`). `last_epoch` / `last_recovery_generation` /
/// `last_canonical_hash` are the wipe-surviving baseline (a numeric value never
/// seen is `0`; a missing hash is the one-time migration state, §6.4/§7.4).
///
/// # Fold/hash acceptance
///
/// | Payload | Fold relation | Stored hash | Result |
/// |---|---|---|---|
/// | normal, generation absent | cached generation = 0 | — | resolve as generation 0 (migration), then apply the normal epoch/hash rows below |
/// | normal, generation absent | cached generation > 0 | any | reject `NormalMissingRecoveryGeneration` (unless the epoch is lower, which rejects first as `StaleEpoch`) |
/// | normal, after generation resolution | epoch lower, or generation lower | any | reject rollback |
/// | normal, after generation resolution | epoch/generation equal | absent | accept once (migration) |
/// | normal, after generation resolution | epoch/generation equal | same | accept exact re-push |
/// | normal, after generation resolution | epoch/generation equal | different | reject conflict |
/// | normal, after generation resolution | epoch or authorised generation higher | any | accept advance |
/// | recovery | generation lower | any | reject replay |
/// | recovery | generation equal | absent | reject (no recovery migration) |
/// | recovery | generation equal | same | accept exact re-push |
/// | recovery | generation equal | different | reject conflict |
/// | recovery | generation higher | any epoch/hash | accept override |
///
/// Recovery's generation is its fold axis and supersedes epoch. A normal
/// payload never gains that override: an older normal epoch still rejects even
/// if it carries a higher recovery baseline.
#[derive(Debug, Clone)]
pub struct NodeTrustState {
    /// `key_id` of the genesis verify key (provisioned out-of-band,
    /// §7.4). MUST also appear in `trusted_keys`.
    pub genesis_key_id: String,
    /// Currently-trusted verify keys (includes genesis).
    pub trusted_keys: Vec<TrustedKey>,
    /// Last accepted mesh epoch — the rollback floor (§7.2).
    pub last_epoch: u64,
    /// Last accepted recovery generation — the recovery replay floor
    /// (§6.4).
    pub last_recovery_generation: u64,
    /// Canonical blake3 of the payload accepted at the current fold position.
    /// `None` is accepted only for migration from the legacy two-field baseline;
    /// the caller persists the accepted payload's hash immediately afterwards.
    pub last_canonical_hash: Option<String>,
}

impl NodeTrustState {
    fn trusted_pubkey(&self, key_id: &str) -> Option<&[u8]> {
        self.trusted_keys
            .iter()
            .find(|k| k.key_id == key_id)
            .map(|k| k.pubkey.as_slice())
    }
}

/// The outcome of a successful [`SignedInventory::verify`]: what the
/// node should adopt and persist. Returned by value; persistence +
/// verify-key installation is the caller's job (1b-c).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedInventory {
    /// The new mesh epoch to persist as the rollback floor.
    pub epoch: u64,
    /// The new recovery generation to persist as the replay floor.
    pub recovery_generation: u64,
    /// `true` when accepted via the genesis recovery override (§6.4).
    pub via_recovery: bool,
    /// `key_id`s of trusted keys whose signatures verified — the §17
    /// authority-plane audit set ("which anchors authorised this").
    pub verified_by: Vec<String>,
    /// The verify-key set to adopt as the new trusted anchor set
    /// (§7.2 in-band adoption) — validated to still contain genesis.
    pub adopt_verify_keys: Vec<VerifyKey>,
}

/// Why an inventory was rejected. Every variant is a hard reject —
/// there is no partial acceptance. Causes map onto the §17
/// `inventory.rejected` event reasons.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VerifyError {
    #[error("unknown canonical_encoding {0:?} (only {CANONICAL_ENCODING_V1:?} is accepted)")]
    UnknownCanonicalEncoding(String),

    #[error(
        "unsupported schema_version {0} (this verifier accepts only {SUPPORTED_SCHEMA_VERSION})"
    )]
    UnsupportedSchemaVersion(u32),

    #[error("malformed payload: {0}")]
    MalformedPayload(&'static str),

    #[error("payload carries no verify_keys")]
    NoVerifyKeys,

    #[error(
        "verify_keys entry {key_id:?} has unsupported key_type {key_type:?} (only {ALG_ED25519:?})"
    )]
    UnsupportedKeyType { key_id: String, key_type: String },

    #[error("verify_keys entry {key_id:?} has a malformed (non-base64 / wrong-length) pubkey")]
    MalformedVerifyKey { key_id: String },

    #[error("duplicate verify_keys key_id {key_id:?}")]
    DuplicateVerifyKeyId { key_id: String },

    #[error("no signature by a declared + currently-trusted verify key verified")]
    NoTrustedSignature,

    #[error(
        "adopted verify_keys would drop the non-removable genesis key {genesis_key_id:?} (§7.2 rule 1)"
    )]
    GenesisDropped { genesis_key_id: String },

    #[error("stale epoch: payload epoch {payload_epoch} < cached {last_epoch} (rollback)")]
    StaleEpoch { payload_epoch: u64, last_epoch: u64 },

    #[error("recovery:true but no genesis signature authorised it (§6.4)")]
    RecoveryNotGenesisSigned,

    #[error(
        "normal inventory carries recovery:false; the recovery key is presence-marked and normal payloads must omit it"
    )]
    NormalExplicitRecoveryFalse,

    #[error("recovery:true but no recovery_generation present (§6.4)")]
    RecoveryMissingGeneration,

    #[error("stale recovery: generation {payload_generation} < cached {last_generation} (replay)")]
    StaleRecovery {
        payload_generation: u64,
        last_generation: u64,
    },

    #[error(
        "recovery re-push at generation {generation} cannot be authenticated without a pinned baseline hash"
    )]
    RecoveryEqualGenerationUnpinned { generation: u64 },

    #[error(
        "recovery_generation {payload_generation} < cached {last_generation} (baseline rollback)"
    )]
    RecoveryGenerationRollback {
        payload_generation: u64,
        last_generation: u64,
    },

    #[error(
        "normal inventory omits recovery_generation while cached generation is {last_generation}; generation-silent payload predates recovery-era baselining"
    )]
    NormalMissingRecoveryGeneration { last_generation: u64 },

    #[error(
        "a non-genesis inventory may not raise recovery_generation \
         {payload_generation} above cached {last_generation} (would lock out genesis recovery, §6.4)"
    )]
    RecoveryFloorRaiseRequiresGenesis {
        payload_generation: u64,
        last_generation: u64,
    },

    #[error(
        "payload at epoch {payload_epoch} / recovery_generation {payload_generation} conflicts with the accepted baseline hash: expected {expected_hash}, got {payload_hash}"
    )]
    BaselineHashMismatch {
        payload_epoch: u64,
        payload_generation: u64,
        expected_hash: String,
        payload_hash: String,
    },
}

/// Decode a base64 string and require an exact byte length.
fn decode_fixed(s: &str, len: usize) -> Option<Vec<u8>> {
    let bytes = B64.decode(s).ok()?;
    (bytes.len() == len).then_some(bytes)
}

impl SignedInventory {
    /// Parse a wire JSON artifact into a [`SignedInventory`]. Unknown
    /// fields (at any documented level) and duplicate object keys in
    /// `members` are hard parse errors — a v2 field or a key-collision
    /// cannot silently bypass the signature contract.
    ///
    /// This is *structural* parse only; call [`Self::verify`] next for
    /// the security checks. The verifier re-canonicalises from the
    /// parsed fields, never from these wire bytes.
    pub fn parse(wire: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(wire)
    }

    /// Verify the inventory against the node's current trust state and,
    /// on success, return what to adopt + persist.
    ///
    /// See the module documentation for the full contract. The order of
    /// checks is deliberate: `canonical_encoding` is validated against
    /// the closed set **before** any canonicalisation (§7.2).
    pub fn verify(&self, state: &NodeTrustState) -> Result<AcceptedInventory, VerifyError> {
        let p = &self.payload;

        // (1) Encoding agility defense — closed set, FIRST (§7.2), so
        //     the self-described field cannot select the canonicaliser.
        if p.canonical_encoding != CANONICAL_ENCODING_V1 {
            return Err(VerifyError::UnknownCanonicalEncoding(
                p.canonical_encoding.clone(),
            ));
        }

        // (2) Schema version — reject unknown (fail-closed, §16).
        if p.schema_version != SUPPORTED_SCHEMA_VERSION {
            return Err(VerifyError::UnsupportedSchemaVersion(p.schema_version));
        }

        // (3) members shape — schema says an array of node-records.
        if !p.members.is_array() {
            return Err(VerifyError::MalformedPayload("members is not an array"));
        }

        // (4) verify_keys well-formed: non-empty, Ed25519, decodable
        //     32-byte pubkeys, distinct key_ids — so the set we may
        //     adopt on accept is never garbage.
        if p.verify_keys.is_empty() {
            return Err(VerifyError::NoVerifyKeys);
        }
        let mut vk_ids = BTreeSet::new();
        for vk in &p.verify_keys {
            if vk.key_type != ALG_ED25519 {
                return Err(VerifyError::UnsupportedKeyType {
                    key_id: vk.key_id.clone(),
                    key_type: vk.key_type.clone(),
                });
            }
            if decode_fixed(&vk.pubkey, ED25519_PUBKEY_LEN).is_none() {
                return Err(VerifyError::MalformedVerifyKey {
                    key_id: vk.key_id.clone(),
                });
            }
            if !vk_ids.insert(vk.key_id.as_str()) {
                return Err(VerifyError::DuplicateVerifyKeyId {
                    key_id: vk.key_id.clone(),
                });
            }
        }

        // (5) Re-canonicalise from PARSED fields, never the wire bytes.
        let msg = p.canonical_bytes();

        // (6) Collect the authoritative signatures.
        //
        // `signatures[]` lives OUTSIDE the signed bytes, so a hostile
        // carrier can freely append, duplicate, or corrupt entries
        // without breaking the payload signature. Every invalid or
        // unrecognised entry is therefore SKIPPED, never a hard reject —
        // hard-rejecting would let one appended junk entry deny a
        // legitimate update. Acceptance instead requires ≥1 genuinely
        // valid authoritative signature. A signature counts only when it:
        //   - uses alg Ed25519 (no downgrade: any other alg never counts),
        //   - names a key_id DECLARED in payload.verify_keys (§7.1), and
        //   - names a key the node ALREADY trusts, verifying against the
        //     TRUSTED pubkey (never the payload-claimed one, §7.2).
        // A signature by a declared-but-not-yet-trusted key is skipped —
        // that is what lets a node trusting only the old key accept a
        // dual-signed rotation inventory and learn the new key (§6.4).
        let declared: BTreeSet<&str> = p.verify_keys.iter().map(|vk| vk.key_id.as_str()).collect();
        let mut verified_by: Vec<String> = Vec::new();
        let mut counted: BTreeSet<&str> = BTreeSet::new();
        let mut genesis_verified = false;
        for s in &self.signatures {
            if s.alg != ALG_ED25519 {
                continue;
            }
            if !declared.contains(s.key_id.as_str()) {
                continue;
            }
            let Some(pubkey) = state.trusted_pubkey(&s.key_id) else {
                continue;
            };
            let Some(sig) = decode_fixed(&s.sig, ED25519_SIG_LEN) else {
                continue;
            };
            if verify_ed25519(pubkey, &sig, &msg).is_err() {
                continue;
            }
            // Count each key_id at most once — no dup-sig fake quorum.
            if counted.insert(s.key_id.as_str()) {
                if s.key_id == state.genesis_key_id {
                    genesis_verified = true;
                }
                verified_by.push(s.key_id.clone());
            }
        }
        if verified_by.is_empty() {
            return Err(VerifyError::NoTrustedSignature);
        }

        // (8) Anti-lockout rule 1 (§7.2): the adopted verify_keys set
        //     MUST still contain genesis — unconditionally, even on
        //     recovery.
        if !p
            .verify_keys
            .iter()
            .any(|vk| vk.key_id == state.genesis_key_id)
        {
            return Err(VerifyError::GenesisDropped {
                genesis_key_id: state.genesis_key_id.clone(),
            });
        }

        // (9) Rollback / recovery axis. Epoch/generation-driven only —
        //     signed_at / valid_until are advisory (§16a) and not read.
        //
        // Apply NodeTrustState's documented fold/hash acceptance table after
        // the signature and verify-key checks above.
        let recovery = match p.recovery {
            None => false,
            Some(true) => true,
            Some(false) => return Err(VerifyError::NormalExplicitRecoveryFalse),
        };
        let accepted_generation = if recovery {
            // Recovery supersedes the epoch axis, but ONLY under the
            // genesis anchor (§6.4). An older generation is a replay; an equal
            // generation is accepted only when the hash pin below proves it is
            // the exact same payload. The legacy hashless migration allowance
            // is deliberately normal-path only: an equal-generation recovery
            // can carry an arbitrary lower epoch and must not regress that floor.
            if !genesis_verified {
                return Err(VerifyError::RecoveryNotGenesisSigned);
            }
            let Some(generation) = p.recovery_generation else {
                return Err(VerifyError::RecoveryMissingGeneration);
            };
            if generation < state.last_recovery_generation {
                return Err(VerifyError::StaleRecovery {
                    payload_generation: generation,
                    last_generation: state.last_recovery_generation,
                });
            }
            if generation == state.last_recovery_generation && state.last_canonical_hash.is_none() {
                return Err(VerifyError::RecoveryEqualGenerationUnpinned { generation });
            }
            generation
        } else {
            // Normal path: epoch MUST be monotonic — reject an OLDER
            // epoch even though the signature verifies (§7.2).
            if p.epoch < state.last_epoch {
                return Err(VerifyError::StaleEpoch {
                    payload_epoch: p.epoch,
                    last_epoch: state.last_epoch,
                });
            }
            // A normal inventory carries the current recovery_generation
            // as a cold-boot / propagation baseline (§6.4). The floor may
            // only ever be RAISED under a genesis signature — otherwise a
            // stolen *active* key could sign a normal inventory with an
            // enormous recovery_generation, pinning the floor so high that
            // a later genesis recovery (which needs generation > floor)
            // can never be issued: a permanent recovery lockout. A
            // non-genesis inventory may therefore only echo the current
            // floor; it may never raise it, and may never roll it back.
            match p.recovery_generation {
                None if state.last_recovery_generation == 0 => 0,
                None => {
                    return Err(VerifyError::NormalMissingRecoveryGeneration {
                        last_generation: state.last_recovery_generation,
                    });
                }
                Some(generation) if generation == state.last_recovery_generation => generation,
                Some(generation) if generation < state.last_recovery_generation => {
                    return Err(VerifyError::RecoveryGenerationRollback {
                        payload_generation: generation,
                        last_generation: state.last_recovery_generation,
                    });
                }
                // generation > floor: a genuine advance (propagating a
                // recovery to a node that missed it, or a cold-boot
                // baseline) — permitted ONLY under genesis authorisation.
                Some(generation) if genesis_verified => generation,
                Some(generation) => {
                    return Err(VerifyError::RecoveryFloorRaiseRequiresGenesis {
                        payload_generation: generation,
                        last_generation: state.last_recovery_generation,
                    });
                }
            }
        };

        let fold_advanced = if recovery {
            accepted_generation > state.last_recovery_generation
        } else {
            p.epoch > state.last_epoch || accepted_generation > state.last_recovery_generation
        };
        if !fold_advanced && let Some(expected_hash) = state.last_canonical_hash.as_ref() {
            let payload_hash = p.canonical_blake3();
            if &payload_hash != expected_hash {
                return Err(VerifyError::BaselineHashMismatch {
                    payload_epoch: p.epoch,
                    payload_generation: accepted_generation,
                    expected_hash: expected_hash.clone(),
                    payload_hash,
                });
            }
        }

        Ok(AcceptedInventory {
            epoch: p.epoch,
            recovery_generation: accepted_generation,
            via_recovery: recovery,
            verified_by,
            adopt_verify_keys: p.verify_keys.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use rand::rngs::OsRng;
    use serde_json::json;

    // ---- builders --------------------------------------------------

    fn keypair() -> (SigningKey, String) {
        let sk = SigningKey::generate(&mut OsRng);
        let pubkey_b64 = B64.encode(sk.verifying_key().to_bytes());
        (sk, pubkey_b64)
    }

    fn trusted(key_id: &str, pubkey_b64: &str, status: KeyStatus) -> TrustedKey {
        TrustedKey {
            key_id: key_id.into(),
            pubkey: B64.decode(pubkey_b64).unwrap(),
            status,
        }
    }

    fn base_payload(verify_keys: Vec<VerifyKey>, epoch: u64) -> InventoryPayload {
        InventoryPayload {
            schema_version: 1,
            canonical_encoding: CANONICAL_ENCODING_V1.into(),
            mesh: "bus".into(),
            subnet: "192.0.2.0/24".into(),
            epoch,
            signed_at: "2026-06-03T00:00:00Z".into(),
            valid_until: "2026-06-10T00:00:00Z".into(),
            hub: vec!["beta".into()],
            verify_keys,
            members: json!([{ "name": "alpha", "bus": true }]),
            recovery: None,
            recovery_generation: None,
        }
    }

    fn vkey(key_id: &str, pubkey_b64: &str, status: KeyStatus) -> VerifyKey {
        VerifyKey {
            key_id: key_id.into(),
            pubkey: pubkey_b64.into(),
            key_type: ALG_ED25519.into(),
            status,
        }
    }

    fn sign(sk: &SigningKey, key_id: &str, payload: &InventoryPayload) -> InvSignature {
        let sig = sk.sign(&payload.canonical_bytes());
        InvSignature {
            key_id: key_id.into(),
            alg: ALG_ED25519.into(),
            sig: B64.encode(sig.to_bytes()),
        }
    }

    /// A single-genesis-key node + a matching valid inventory at the
    /// given epoch. The node's baseline is epoch 0 / generation 0.
    fn genesis_fixture(epoch: u64) -> (SigningKey, NodeTrustState, InventoryPayload) {
        let (gk, gpub) = keypair();
        let state = NodeTrustState {
            genesis_key_id: "genesis".into(),
            trusted_keys: vec![trusted("genesis", &gpub, KeyStatus::Active)],
            last_epoch: 0,
            last_recovery_generation: 0,
            last_canonical_hash: None,
        };
        let payload = base_payload(vec![vkey("genesis", &gpub, KeyStatus::Active)], epoch);
        (gk, state, payload)
    }

    // ---- happy path ------------------------------------------------

    #[test]
    fn accepts_a_validly_signed_inventory() {
        let (gk, state, payload) = genesis_fixture(1);
        let inv = SignedInventory {
            signatures: vec![sign(&gk, "genesis", &payload)],
            payload,
        };
        let accepted = inv.verify(&state).expect("should accept");
        assert_eq!(accepted.epoch, 1);
        assert_eq!(accepted.recovery_generation, 0);
        assert!(!accepted.via_recovery);
        assert_eq!(accepted.verified_by, vec!["genesis".to_string()]);
        assert_eq!(accepted.adopt_verify_keys.len(), 1);
    }

    #[test]
    fn accepts_equal_epoch_reapply() {
        // Equal fold coordinates are a harmless re-push only when the
        // canonical payload hash is identical.
        let (gk, mut state, payload) = genesis_fixture(5);
        state.last_epoch = 5;
        state.last_canonical_hash = Some(payload.canonical_blake3());
        let inv = SignedInventory {
            signatures: vec![sign(&gk, "genesis", &payload)],
            payload,
        };
        assert!(inv.verify(&state).is_ok());
    }

    #[test]
    fn d2_credential_survives_the_signed_inventory_pipeline() {
        // SPEC 13 2-b end-to-end: `members` is opaque, so a kind:"d2" credential
        // authored into a member-record is covered by the signed canonical bytes
        // (no signer code needed). Prove the full loop: sign -> verify ->
        // select_d2_pubkeys -> the node's prover signature admits via admit().
        let (gk, state, mut payload) = genesis_fixture(1);
        let (node_sk, d2_pub) = keypair();
        payload.members = json!([{
            "name": "delta",
            "status": "active",
            "bus": true,
            "credentials": [
                { "kind": "wg", "pubkey": "AAAA", "from_epoch": 1, "until_epoch": null },
                { "kind": "d2", "pubkey": d2_pub, "from_epoch": 1, "until_epoch": null },
            ],
        }]);
        let mut inv = SignedInventory {
            signatures: vec![sign(&gk, "genesis", &payload)],
            payload,
        };
        // The d2-bearing inventory verifies (the credential is in the signature).
        let accepted = inv
            .verify(&state)
            .expect("d2-bearing inventory should verify");
        assert_eq!(accepted.epoch, 1);

        // The d2 pubkey is selectable from the (opaque) member record...
        let member = inv.payload.members.as_array().unwrap()[0].clone();
        assert_eq!(
            crate::admission::select_d2_pubkeys(&member, 1),
            vec![B64.decode(&d2_pub).unwrap()]
        );
        // ...and a transcript the node signs with its d2 PRIVATE key admits.
        let t = crate::admission::AdmissionTranscript {
            mesh_fqdn: "bus".into(),
            claimed_source_node: "delta".into(),
            verifying_broker_node: "beta".into(),
            inventory_epoch: 1,
            session_id: vec![1, 2, 3],
            server_nonce: vec![5u8; 32],
            client_ephemeral: vec![0u8; 32],
            channel_binding_hash: vec![0u8; 32],
        };
        let sig = crate::admission::sign_admission_transcript(&node_sk.to_bytes(), &t).unwrap();
        assert_eq!(crate::admission::admit(&member, &t, &sig, 1), Ok(()));

        // The d2 credential is COVERED BY THE SIGNATURE, not an unsigned side
        // field: tampering the signed d2 pubkey makes verification fail (members
        // are in the signed canonical bytes). Without this, the test would still
        // pass if `members` were dropped from `canonical_bytes()`.
        inv.payload.members[0]["credentials"][1]["pubkey"] = json!(B64.encode([7u8; 32]));
        assert!(
            inv.verify(&state).is_err(),
            "tampering a signed member credential must break verification"
        );
    }

    // ---- signature handling (skip-don't-reject; §7.1 declared) -----

    #[test]
    fn rejects_signature_by_untrusted_key_id() {
        // The signature is valid but its key_id is neither declared in
        // verify_keys nor trusted → skipped → NoTrustedSignature.
        let (gk, state, payload) = genesis_fixture(1);
        let inv = SignedInventory {
            signatures: vec![sign(&gk, "rogue", &payload)],
            payload,
        };
        assert_eq!(inv.verify(&state), Err(VerifyError::NoTrustedSignature));
    }

    #[test]
    fn skips_sig_naming_trusted_key_but_signed_by_other_key() {
        // Claims key_id "genesis" but is signed by a different key — it
        // does not verify against the trusted genesis pubkey, so it is
        // skipped (not a hard reject: signatures are malleable). With no
        // other valid signature → NoTrustedSignature.
        let (_gk, state, payload) = genesis_fixture(1);
        let (other, _) = keypair();
        let inv = SignedInventory {
            signatures: vec![sign(&other, "genesis", &payload)],
            payload,
        };
        assert_eq!(inv.verify(&state), Err(VerifyError::NoTrustedSignature));
    }

    #[test]
    fn rejects_tampered_payload() {
        // A tampered payload changes the canonical bytes, so the genesis
        // signature no longer verifies → skipped → NoTrustedSignature.
        let (gk, state, payload) = genesis_fixture(1);
        let sig = sign(&gk, "genesis", &payload);
        let mut tampered = payload;
        tampered.subnet = "10.0.0.0/8".into(); // changes canonical bytes
        let inv = SignedInventory {
            signatures: vec![sig],
            payload: tampered,
        };
        assert_eq!(inv.verify(&state), Err(VerifyError::NoTrustedSignature));
    }

    #[test]
    fn non_ed25519_alg_does_not_count() {
        // A non-Ed25519 alg never counts (no downgrade). With it as the
        // only signature → NoTrustedSignature.
        for alg in ["RSA", "none", "ed25519" /* case-sensitive */] {
            let (gk, state, payload) = genesis_fixture(1);
            let mut sig = sign(&gk, "genesis", &payload);
            sig.alg = alg.into();
            let inv = SignedInventory {
                signatures: vec![sig],
                payload,
            };
            assert_eq!(
                inv.verify(&state),
                Err(VerifyError::NoTrustedSignature),
                "alg {alg:?} must not count"
            );
        }
    }

    #[test]
    fn duplicate_signature_is_deduped_not_a_quorum() {
        // Two identical valid genesis signatures: the second is a
        // duplicate key_id and is not counted twice — but the inventory
        // is still ACCEPTED on the single distinct authoriser.
        let (gk, state, payload) = genesis_fixture(1);
        let s = sign(&gk, "genesis", &payload);
        let inv = SignedInventory {
            signatures: vec![s.clone(), s],
            payload,
        };
        let accepted = inv.verify(&state).expect("dedup, still valid");
        assert_eq!(accepted.verified_by, vec!["genesis".to_string()]);
    }

    #[test]
    fn empty_signatures_yield_no_trusted_signature() {
        let (_gk, state, payload) = genesis_fixture(1);
        let inv = SignedInventory {
            signatures: vec![],
            payload,
        };
        assert_eq!(inv.verify(&state), Err(VerifyError::NoTrustedSignature));
    }

    #[test]
    fn tolerates_appended_junk_signatures() {
        // A valid genesis signature PLUS three appended junk entries (a
        // hostile carrier can append freely — sigs are outside the signed
        // bytes). The inventory must still verify on the genuine one.
        let (gk, state, payload) = genesis_fixture(1);
        let good = sign(&gk, "genesis", &payload);
        let junk_bad_bytes = InvSignature {
            key_id: "genesis".into(),
            alg: ALG_ED25519.into(),
            sig: "not-base64-!!!".into(),
        };
        let junk_bad_alg = InvSignature {
            key_id: "genesis".into(),
            alg: "RSA".into(),
            sig: good.sig.clone(),
        };
        let junk_undeclared = InvSignature {
            key_id: "phantom".into(),
            alg: ALG_ED25519.into(),
            sig: good.sig.clone(),
        };
        let inv = SignedInventory {
            signatures: vec![junk_bad_bytes, junk_bad_alg, good, junk_undeclared],
            payload,
        };
        let accepted = inv.verify(&state).expect("genuine sig survives junk");
        assert_eq!(accepted.verified_by, vec!["genesis".to_string()]);
    }

    #[test]
    fn rejects_signature_key_id_not_declared_in_verify_keys() {
        // §7.1: an authorising key_id must be declared in verify_keys. The
        // node trusts both genesis + op, but the payload declares only
        // genesis; an op-signed inventory (op trusted but UNDECLARED) is
        // therefore not authoritative → NoTrustedSignature.
        let (_gk, gpub) = keypair();
        let (op, opub) = keypair();
        let state = NodeTrustState {
            genesis_key_id: "genesis".into(),
            trusted_keys: vec![
                trusted("genesis", &gpub, KeyStatus::Active),
                trusted("op", &opub, KeyStatus::Active),
            ],
            last_epoch: 0,
            last_recovery_generation: 0,
            last_canonical_hash: None,
        };
        // verify_keys declares only genesis (op is trusted but not declared).
        let payload = base_payload(vec![vkey("genesis", &gpub, KeyStatus::Active)], 2);
        let inv = SignedInventory {
            signatures: vec![sign(&op, "op", &payload)],
            payload,
        };
        assert_eq!(inv.verify(&state), Err(VerifyError::NoTrustedSignature));
    }

    // ---- verify_keys validation -----------------------------------

    #[test]
    fn rejects_unknown_canonical_encoding_first() {
        // Even with everything else valid, a bad encoding is the very
        // first rejection (before canonicalising).
        let (gk, state, mut payload) = genesis_fixture(1);
        payload.canonical_encoding = "json/evil".into();
        let inv = SignedInventory {
            signatures: vec![sign(&gk, "genesis", &payload)],
            payload,
        };
        assert_eq!(
            inv.verify(&state),
            Err(VerifyError::UnknownCanonicalEncoding("json/evil".into()))
        );
    }

    #[test]
    fn rejects_unsupported_schema_version() {
        let (gk, state, mut payload) = genesis_fixture(1);
        payload.schema_version = 2;
        let inv = SignedInventory {
            signatures: vec![sign(&gk, "genesis", &payload)],
            payload,
        };
        assert_eq!(
            inv.verify(&state),
            Err(VerifyError::UnsupportedSchemaVersion(2))
        );
    }

    #[test]
    fn rejects_unsupported_key_type() {
        let (gk, gpub) = keypair();
        let state = NodeTrustState {
            genesis_key_id: "genesis".into(),
            trusted_keys: vec![trusted("genesis", &gpub, KeyStatus::Active)],
            last_epoch: 0,
            last_recovery_generation: 0,
            last_canonical_hash: None,
        };
        let mut payload = base_payload(vec![vkey("genesis", &gpub, KeyStatus::Active)], 1);
        payload.verify_keys[0].key_type = "RSA".into();
        let inv = SignedInventory {
            signatures: vec![sign(&gk, "genesis", &payload)],
            payload,
        };
        assert!(matches!(
            inv.verify(&state),
            Err(VerifyError::UnsupportedKeyType { .. })
        ));
    }

    #[test]
    fn rejects_malformed_verify_key_pubkey() {
        let (gk, gpub) = keypair();
        let state = NodeTrustState {
            genesis_key_id: "genesis".into(),
            trusted_keys: vec![trusted("genesis", &gpub, KeyStatus::Active)],
            last_epoch: 0,
            last_recovery_generation: 0,
            last_canonical_hash: None,
        };
        let mut payload = base_payload(vec![vkey("genesis", &gpub, KeyStatus::Active)], 1);
        payload.verify_keys[0].pubkey = "AAAA".into(); // valid base64, wrong length
        let inv = SignedInventory {
            signatures: vec![sign(&gk, "genesis", &payload)],
            payload,
        };
        assert!(matches!(
            inv.verify(&state),
            Err(VerifyError::MalformedVerifyKey { .. })
        ));
    }

    #[test]
    fn rejects_empty_verify_keys() {
        let (gk, _gpub) = keypair();
        let state = NodeTrustState {
            genesis_key_id: "genesis".into(),
            trusted_keys: vec![],
            last_epoch: 0,
            last_recovery_generation: 0,
            last_canonical_hash: None,
        };
        let payload = base_payload(vec![], 1);
        let inv = SignedInventory {
            signatures: vec![sign(&gk, "genesis", &payload)],
            payload,
        };
        assert_eq!(inv.verify(&state), Err(VerifyError::NoVerifyKeys));
    }

    // ---- epoch monotonicity ---------------------------------------

    #[test]
    fn rejects_stale_epoch() {
        let (gk, mut state, payload) = genesis_fixture(3);
        state.last_epoch = 10; // node has already seen a higher epoch
        let inv = SignedInventory {
            signatures: vec![sign(&gk, "genesis", &payload)],
            payload,
        };
        assert_eq!(
            inv.verify(&state),
            Err(VerifyError::StaleEpoch {
                payload_epoch: 3,
                last_epoch: 10
            })
        );
    }

    // ---- genesis anti-lockout (rule 1) ----------------------------

    #[test]
    fn rejects_dropping_genesis_key() {
        // The node trusts genesis + an active op key. A new inventory,
        // validly signed by the op key, drops genesis from verify_keys
        // → rule 1 rejects it (genesis is non-removable in-band).
        let (gk, gpub) = keypair();
        let (ok_key, opub) = keypair();
        let state = NodeTrustState {
            genesis_key_id: "genesis".into(),
            trusted_keys: vec![
                trusted("genesis", &gpub, KeyStatus::Active),
                trusted("op", &opub, KeyStatus::Active),
            ],
            last_epoch: 0,
            last_recovery_generation: 0,
            last_canonical_hash: None,
        };
        let _ = gk; // genesis not used to sign here
        // verify_keys drops genesis, keeps only "op".
        let payload = base_payload(vec![vkey("op", &opub, KeyStatus::Active)], 2);
        let inv = SignedInventory {
            signatures: vec![sign(&ok_key, "op", &payload)],
            payload,
        };
        assert_eq!(
            inv.verify(&state),
            Err(VerifyError::GenesisDropped {
                genesis_key_id: "genesis".into()
            })
        );
    }

    // ---- in-band rotation tolerance -------------------------------

    #[test]
    fn accepts_dual_signed_rotation_trusting_only_old_key() {
        // Node trusts only the old genesis key. A rotation inventory is
        // dual-signed (old + new) and carries both keys in verify_keys.
        // The node accepts on the old signature and learns the new key.
        let (gk, gpub) = keypair();
        let (newk, npub) = keypair();
        let state = NodeTrustState {
            genesis_key_id: "genesis".into(),
            trusted_keys: vec![trusted("genesis", &gpub, KeyStatus::Active)],
            last_epoch: 0,
            last_recovery_generation: 0,
            last_canonical_hash: None,
        };
        let payload = base_payload(
            vec![
                vkey("genesis", &gpub, KeyStatus::Active),
                vkey("opkey-2026q3", &npub, KeyStatus::Active),
            ],
            2,
        );
        let inv = SignedInventory {
            signatures: vec![
                sign(&gk, "genesis", &payload),
                sign(&newk, "opkey-2026q3", &payload),
            ],
            payload,
        };
        let accepted = inv.verify(&state).expect("should accept on old key");
        // Only the trusted (genesis) signature counts toward verified_by;
        // the not-yet-trusted new key is skipped, not failed.
        assert_eq!(accepted.verified_by, vec!["genesis".to_string()]);
        assert_eq!(accepted.adopt_verify_keys.len(), 2);
    }

    // ---- recovery override (§6.4) ---------------------------------

    #[test]
    fn recovery_overrides_a_higher_attacker_epoch() {
        // Attacker (stolen active key) pushed epoch 100; the node
        // cached it. The operator's genesis recovery inventory carries a
        // LOWER epoch (re-baselining) but recovery:true + a fresh
        // recovery_generation, signed by genesis → MUST be accepted
        // despite the lower epoch.
        let (gk, gpub) = keypair();
        let state = NodeTrustState {
            genesis_key_id: "genesis".into(),
            trusted_keys: vec![trusted("genesis", &gpub, KeyStatus::Active)],
            last_epoch: 100,
            last_recovery_generation: 0,
            last_canonical_hash: None,
        };
        let mut payload = base_payload(vec![vkey("genesis", &gpub, KeyStatus::Active)], 2);
        payload.recovery = Some(true);
        payload.recovery_generation = Some(1);
        let inv = SignedInventory {
            signatures: vec![sign(&gk, "genesis", &payload)],
            payload,
        };
        let accepted = inv.verify(&state).expect("recovery should override epoch");
        assert!(accepted.via_recovery);
        assert_eq!(accepted.epoch, 2);
        assert_eq!(accepted.recovery_generation, 1);
    }

    #[test]
    fn rejects_stale_recovery_replay() {
        // The node already advanced to recovery_generation 5. Replaying
        // an older genesis-signed recovery (generation 3) must fail.
        let (gk, gpub) = keypair();
        let state = NodeTrustState {
            genesis_key_id: "genesis".into(),
            trusted_keys: vec![trusted("genesis", &gpub, KeyStatus::Active)],
            last_epoch: 100,
            last_recovery_generation: 5,
            last_canonical_hash: None,
        };
        let mut payload = base_payload(vec![vkey("genesis", &gpub, KeyStatus::Active)], 2);
        payload.recovery = Some(true);
        payload.recovery_generation = Some(3);
        let inv = SignedInventory {
            signatures: vec![sign(&gk, "genesis", &payload)],
            payload,
        };
        assert_eq!(
            inv.verify(&state),
            Err(VerifyError::StaleRecovery {
                payload_generation: 3,
                last_generation: 5
            })
        );
    }

    #[test]
    fn accepts_equal_recovery_generation_exact_repush() {
        // Recovery uses generation as its fold axis. An equal generation is
        // accepted only for the exact payload already pinned in the baseline.
        let (gk, gpub) = keypair();
        let mut payload = base_payload(vec![vkey("genesis", &gpub, KeyStatus::Active)], 1);
        payload.recovery = Some(true);
        payload.recovery_generation = Some(2);
        let state = NodeTrustState {
            genesis_key_id: "genesis".into(),
            trusted_keys: vec![trusted("genesis", &gpub, KeyStatus::Active)],
            last_epoch: 1,
            last_recovery_generation: 2,
            last_canonical_hash: Some(payload.canonical_blake3()),
        };
        let inv = SignedInventory {
            signatures: vec![sign(&gk, "genesis", &payload)],
            payload,
        };
        assert!(inv.verify(&state).is_ok());
    }

    #[test]
    fn hash_fold_acceptance_table_covers_every_row() {
        let (gk, gpub) = keypair();
        let vkeys = vec![vkey("genesis", &gpub, KeyStatus::Active)];
        let trusted_keys = vec![trusted("genesis", &gpub, KeyStatus::Active)];
        let signed = |payload: InventoryPayload| SignedInventory {
            signatures: vec![sign(&gk, "genesis", &payload)],
            payload,
        };
        let state = |epoch, generation, hash| NodeTrustState {
            genesis_key_id: "genesis".into(),
            trusted_keys: trusted_keys.clone(),
            last_epoch: epoch,
            last_recovery_generation: generation,
            last_canonical_hash: hash,
        };

        let mut normal = base_payload(vkeys.clone(), 5);
        normal.recovery_generation = Some(2);
        let normal_hash = normal.canonical_blake3();

        // normal: lower epoch rejects regardless of hash.
        let mut lower_epoch = normal.clone();
        lower_epoch.epoch = 4;
        assert!(matches!(
            signed(lower_epoch).verify(&state(5, 2, Some(normal_hash.clone()))),
            Err(VerifyError::StaleEpoch { .. })
        ));

        // normal: lower generation rejects regardless of hash.
        let mut lower_generation = normal.clone();
        lower_generation.recovery_generation = Some(1);
        assert!(matches!(
            signed(lower_generation).verify(&state(5, 2, Some(normal_hash.clone()))),
            Err(VerifyError::RecoveryGenerationRollback { .. })
        ));

        // normal: equal fold with no hash accepts once for baseline migration.
        assert!(signed(normal.clone()).verify(&state(5, 2, None)).is_ok());

        // normal: equal fold + same hash accepts an exact re-push.
        assert!(
            signed(normal.clone())
                .verify(&state(5, 2, Some(normal_hash.clone())))
                .is_ok()
        );

        // normal: equal fold + different hash is a conflict.
        let mut normal_conflict = normal.clone();
        normal_conflict.members = json!([{ "name": "different", "bus": true }]);
        assert!(matches!(
            signed(normal_conflict).verify(&state(5, 2, Some(normal_hash.clone()))),
            Err(VerifyError::BaselineHashMismatch { .. })
        ));

        // normal: a higher epoch adopts a new hash.
        let mut higher_epoch = normal.clone();
        higher_epoch.epoch = 6;
        assert!(
            signed(higher_epoch)
                .verify(&state(5, 2, Some("old-hash".into())))
                .is_ok()
        );

        // normal: an authorised higher generation also adopts a new hash.
        let mut higher_generation = normal.clone();
        higher_generation.recovery_generation = Some(3);
        assert!(
            signed(higher_generation)
                .verify(&state(5, 2, Some("old-hash".into())))
                .is_ok()
        );

        let mut recovery = base_payload(vkeys, 2);
        recovery.recovery = Some(true);
        recovery.recovery_generation = Some(2);
        let recovery_hash = recovery.canonical_blake3();

        // recovery: lower generation rejects even though its epoch is arbitrary.
        let mut lower_recovery = recovery.clone();
        lower_recovery.recovery_generation = Some(1);
        assert!(matches!(
            signed(lower_recovery).verify(&state(100, 2, Some(recovery_hash.clone()))),
            Err(VerifyError::StaleRecovery { .. })
        ));

        // recovery: equal generation with no hash rejects; migration is normal-only.
        assert_eq!(
            signed(recovery.clone()).verify(&state(100, 2, None)),
            Err(VerifyError::RecoveryEqualGenerationUnpinned { generation: 2 })
        );

        // recovery: equal generation + same hash accepts the exact re-push.
        assert!(
            signed(recovery.clone())
                .verify(&state(100, 2, Some(recovery_hash.clone())))
                .is_ok()
        );

        // recovery: equal generation + different hash is a conflict.
        let mut recovery_conflict = recovery.clone();
        recovery_conflict.members = json!([{ "name": "different", "bus": true }]);
        assert!(matches!(
            signed(recovery_conflict).verify(&state(100, 2, Some(recovery_hash))),
            Err(VerifyError::BaselineHashMismatch { .. })
        ));

        // recovery: higher generation supersedes epoch and adopts a new hash.
        let mut higher_recovery = recovery;
        higher_recovery.epoch = 1;
        higher_recovery.recovery_generation = Some(3);
        assert!(
            signed(higher_recovery)
                .verify(&state(100, 2, Some("old-hash".into())))
                .is_ok()
        );
    }

    #[test]
    fn rejects_recovery_not_signed_by_genesis() {
        // A recovery inventory signed only by a non-genesis active key
        // must NOT bypass the epoch axis — recovery is genesis-only.
        let (_gk, gpub) = keypair();
        let (ok_key, opub) = keypair();
        let state = NodeTrustState {
            genesis_key_id: "genesis".into(),
            trusted_keys: vec![
                trusted("genesis", &gpub, KeyStatus::Active),
                trusted("op", &opub, KeyStatus::Active),
            ],
            last_epoch: 100,
            last_recovery_generation: 0,
            last_canonical_hash: None,
        };
        let mut payload = base_payload(
            vec![
                vkey("genesis", &gpub, KeyStatus::Active),
                vkey("op", &opub, KeyStatus::Active),
            ],
            2,
        );
        payload.recovery = Some(true);
        payload.recovery_generation = Some(1);
        let inv = SignedInventory {
            signatures: vec![sign(&ok_key, "op", &payload)],
            payload,
        };
        assert_eq!(
            inv.verify(&state),
            Err(VerifyError::RecoveryNotGenesisSigned)
        );
    }

    #[test]
    fn rejects_recovery_without_generation() {
        let (gk, gpub) = keypair();
        let state = NodeTrustState {
            genesis_key_id: "genesis".into(),
            trusted_keys: vec![trusted("genesis", &gpub, KeyStatus::Active)],
            last_epoch: 100,
            last_recovery_generation: 0,
            last_canonical_hash: None,
        };
        let mut payload = base_payload(vec![vkey("genesis", &gpub, KeyStatus::Active)], 2);
        payload.recovery = Some(true);
        payload.recovery_generation = None;
        let inv = SignedInventory {
            signatures: vec![sign(&gk, "genesis", &payload)],
            payload,
        };
        assert_eq!(
            inv.verify(&state),
            Err(VerifyError::RecoveryMissingGeneration)
        );
    }

    #[test]
    fn genesis_signed_normal_inventory_may_raise_recovery_baseline() {
        // A GENESIS-signed normal (non-recovery) inventory may carry a
        // higher recovery_generation to catch a freshly-imaged node up or
        // propagate a recovery; the accepted floor advances.
        let (gk, gpub) = keypair();
        let state = NodeTrustState {
            genesis_key_id: "genesis".into(),
            trusted_keys: vec![trusted("genesis", &gpub, KeyStatus::Active)],
            last_epoch: 0,
            last_recovery_generation: 0,
            last_canonical_hash: None,
        };
        let mut payload = base_payload(vec![vkey("genesis", &gpub, KeyStatus::Active)], 7);
        payload.recovery_generation = Some(3); // genesis-authorised advance
        let inv = SignedInventory {
            signatures: vec![sign(&gk, "genesis", &payload)],
            payload,
        };
        let accepted = inv
            .verify(&state)
            .expect("genesis baseline raise should accept");
        assert!(!accepted.via_recovery);
        assert_eq!(accepted.recovery_generation, 3);
    }

    #[test]
    fn generation_silent_normal_inventory_is_accepted_at_generation_zero() {
        let (gk, state, payload) = genesis_fixture(7);
        assert_eq!(state.last_recovery_generation, 0);
        assert_eq!(payload.recovery_generation, None);
        let inv = SignedInventory {
            signatures: vec![sign(&gk, "genesis", &payload)],
            payload,
        };

        let accepted = inv
            .verify(&state)
            .expect("generation-zero migration payload should remain valid");
        assert_eq!(accepted.recovery_generation, 0);
    }

    #[test]
    fn generation_silent_normal_inventory_is_refused_above_generation_zero() {
        let (gk, mut state, payload) = genesis_fixture(7);
        state.last_recovery_generation = 1;
        let inv = SignedInventory {
            signatures: vec![sign(&gk, "genesis", &payload)],
            payload,
        };

        assert_eq!(
            inv.verify(&state),
            Err(VerifyError::NormalMissingRecoveryGeneration { last_generation: 1 })
        );
    }

    #[test]
    fn explicit_generation_zero_is_accepted_at_generation_zero() {
        let (gk, state, mut payload) = genesis_fixture(7);
        payload.recovery_generation = Some(0);
        let inv = SignedInventory {
            signatures: vec![sign(&gk, "genesis", &payload)],
            payload,
        };

        let accepted = inv
            .verify(&state)
            .expect("an explicit generation-zero baseline should be valid");
        assert_eq!(accepted.recovery_generation, 0);
    }

    #[test]
    fn normal_inventory_refuses_explicit_recovery_false() {
        let (gk, state, mut payload) = genesis_fixture(7);
        payload.recovery = Some(false);
        let inv = SignedInventory {
            signatures: vec![sign(&gk, "genesis", &payload)],
            payload,
        };

        assert_eq!(
            inv.verify(&state),
            Err(VerifyError::NormalExplicitRecoveryFalse)
        );
    }

    #[test]
    fn rejects_non_genesis_raising_recovery_baseline() {
        // BLOCKER guard: a stolen *active* key must NOT be able to pin the
        // recovery floor high via a normal inventory, which would lock out
        // future genesis recovery (which needs generation > floor). The
        // node trusts genesis + op; an op-signed (non-genesis) normal
        // inventory at a valid epoch tries to raise the floor 0 → MAX.
        let (_gk, gpub) = keypair();
        let (op, opub) = keypair();
        let state = NodeTrustState {
            genesis_key_id: "genesis".into(),
            trusted_keys: vec![
                trusted("genesis", &gpub, KeyStatus::Active),
                trusted("op", &opub, KeyStatus::Active),
            ],
            last_epoch: 0,
            last_recovery_generation: 0,
            last_canonical_hash: None,
        };
        let mut payload = base_payload(
            vec![
                vkey("genesis", &gpub, KeyStatus::Active),
                vkey("op", &opub, KeyStatus::Active),
            ],
            1,
        );
        payload.recovery_generation = Some(u64::MAX);
        let inv = SignedInventory {
            signatures: vec![sign(&op, "op", &payload)],
            payload,
        };
        assert_eq!(
            inv.verify(&state),
            Err(VerifyError::RecoveryFloorRaiseRequiresGenesis {
                payload_generation: u64::MAX,
                last_generation: 0,
            })
        );
    }

    #[test]
    fn non_genesis_normal_inventory_may_echo_recovery_baseline() {
        // The counterpart to the BLOCKER guard: a non-genesis key MAY echo
        // the current floor (no raise) — the common post-recovery steady
        // state where day-to-day inventories carry the established floor.
        let (_gk, gpub) = keypair();
        let (op, opub) = keypair();
        let state = NodeTrustState {
            genesis_key_id: "genesis".into(),
            trusted_keys: vec![
                trusted("genesis", &gpub, KeyStatus::Active),
                trusted("op", &opub, KeyStatus::Active),
            ],
            last_epoch: 0,
            last_recovery_generation: 4,
            last_canonical_hash: None,
        };
        let mut payload = base_payload(
            vec![
                vkey("genesis", &gpub, KeyStatus::Active),
                vkey("op", &opub, KeyStatus::Active),
            ],
            1,
        );
        payload.recovery_generation = Some(4); // echo, not a raise
        let inv = SignedInventory {
            signatures: vec![sign(&op, "op", &payload)],
            payload,
        };
        let accepted = inv.verify(&state).expect("echoing the floor is fine");
        assert_eq!(accepted.recovery_generation, 4);
    }

    #[test]
    fn rejects_normal_inventory_rolling_recovery_baseline_back() {
        // After recovery (floor = 5), a normal inventory carrying an
        // older recovery_generation (2) must not roll the floor back.
        let (gk, gpub) = keypair();
        let state = NodeTrustState {
            genesis_key_id: "genesis".into(),
            trusted_keys: vec![trusted("genesis", &gpub, KeyStatus::Active)],
            last_epoch: 0,
            last_recovery_generation: 5,
            last_canonical_hash: None,
        };
        let mut payload = base_payload(vec![vkey("genesis", &gpub, KeyStatus::Active)], 7);
        payload.recovery_generation = Some(2);
        let inv = SignedInventory {
            signatures: vec![sign(&gk, "genesis", &payload)],
            payload,
        };
        assert_eq!(
            inv.verify(&state),
            Err(VerifyError::RecoveryGenerationRollback {
                payload_generation: 2,
                last_generation: 5
            })
        );
    }

    #[test]
    fn recovery_generation_blocks_higher_epoch_generation_silent_replay() {
        let (gk, gpub) = keypair();
        let vkeys = vec![vkey("genesis", &gpub, KeyStatus::Active)];
        let trusted_keys = vec![trusted("genesis", &gpub, KeyStatus::Active)];

        // This genuine historical normal payload predates recovery-generation
        // propagation and has a higher epoch than the later recovery.
        let historical = base_payload(vkeys.clone(), 40);
        let historical_inv = SignedInventory {
            signatures: vec![sign(&gk, "genesis", &historical)],
            payload: historical.clone(),
        };

        let before_recovery = NodeTrustState {
            genesis_key_id: "genesis".into(),
            trusted_keys: trusted_keys.clone(),
            last_epoch: 40,
            last_recovery_generation: 0,
            last_canonical_hash: Some(historical.canonical_blake3()),
        };
        historical_inv
            .verify(&before_recovery)
            .expect("the generation-silent historical payload was valid before recovery");
        let mut recovery = base_payload(vkeys, 9);
        recovery.recovery = Some(true);
        recovery.recovery_generation = Some(1);
        let recovery_hash = recovery.canonical_blake3();
        let recovery_inv = SignedInventory {
            signatures: vec![sign(&gk, "genesis", &recovery)],
            payload: recovery,
        };
        let accepted_recovery = recovery_inv
            .verify(&before_recovery)
            .expect("fresh recovery generation should override the higher epoch");
        assert_eq!(accepted_recovery.epoch, 9);
        assert_eq!(accepted_recovery.recovery_generation, 1);

        let after_recovery = NodeTrustState {
            genesis_key_id: "genesis".into(),
            trusted_keys,
            last_epoch: accepted_recovery.epoch,
            last_recovery_generation: accepted_recovery.recovery_generation,
            last_canonical_hash: Some(recovery_hash),
        };
        assert_eq!(
            historical_inv.verify(&after_recovery),
            Err(VerifyError::NormalMissingRecoveryGeneration { last_generation: 1 })
        );
    }

    // ---- canonical-bytes golden vector ----------------------------

    #[test]
    fn canonical_bytes_golden_vector() {
        // Pins the exact signed-bytes format so a future refactor cannot
        // silently shift the bytes (SPEC 13 §6 risk). 32 zero bytes is a
        // deterministic, valid-length base64 pubkey.
        let pubkey = B64.encode([0u8; 32]);
        let payload = base_payload(vec![vkey("genesis", &pubkey, KeyStatus::Active)], 7);
        let got = String::from_utf8(payload.canonical_bytes()).unwrap();
        let expected = format!(
            concat!(
                "{{",
                r#""canonical_encoding":"json/rfc8785","#,
                r#""epoch":7,"#,
                r#""hub":["beta"],"#,
                r#""members":[{{"bus":true,"name":"alpha"}}],"#,
                r#""mesh":"bus","#,
                r#""schema_version":1,"#,
                r#""signed_at":"2026-06-03T00:00:00Z","#,
                r#""subnet":"192.0.2.0/24","#,
                r#""valid_until":"2026-06-10T00:00:00Z","#,
                r#""verify_keys":[{{"key_id":"genesis","key_type":"Ed25519","pubkey":"{}","status":"active"}}]"#,
                "}}"
            ),
            pubkey
        );
        assert_eq!(got, expected);
    }

    #[test]
    fn authoring_hash_is_stable_across_resigning_metadata() {
        // These are the two payloads produced by signing the SAME authored
        // semantics at different times. The node identity must change because
        // signing metadata is covered by the signature; the authoring identity
        // must not, because committed derived artefacts key off authored
        // semantics rather than the signing ceremony.
        let pubkey = B64.encode([0u8; 32]);
        let first = base_payload(vec![vkey("genesis", &pubkey, KeyStatus::Active)], 7);
        let mut second = first.clone();
        second.signed_at = "2026-07-03T00:00:00Z".into();

        let signing_key = SigningKey::from_bytes(&[7u8; 32]);
        let first_signature = sign(&signing_key, "genesis", &first);
        let second_signature = sign(&signing_key, "genesis", &second);

        assert_ne!(first.canonical_blake3(), second.canonical_blake3());
        assert_ne!(first_signature.sig, second_signature.sig);
        assert_eq!(first.authoring_blake3(), second.authoring_blake3());

        // This is not a tautology over metadata stripping: a real authored
        // member mutation must move the authoring identity.
        second.members[0]["bus"] = json!(false);
        assert_ne!(first.authoring_blake3(), second.authoring_blake3());
    }

    #[test]
    fn canonical_blake3_golden_vector() {
        let pubkey = B64.encode([0u8; 32]);
        let payload = base_payload(vec![vkey("genesis", &pubkey, KeyStatus::Active)], 7);
        assert_eq!(
            payload.canonical_blake3(),
            "e923bbc3dd980eb8db24c94a242fc44fbd457d44efcc550c06c1dd0cdef4bc35"
        );
    }

    #[test]
    fn canonical_bytes_omit_recovery_fields_when_absent() {
        // A normal inventory's canonical bytes must not contain the
        // recovery keys at all (skip_serializing_if), so they never
        // perturb a non-recovery signature.
        let (_gk, _gpub) = keypair();
        let payload = base_payload(
            vec![vkey("genesis", &B64.encode([0u8; 32]), KeyStatus::Active)],
            1,
        );
        let s = String::from_utf8(payload.canonical_bytes()).unwrap();
        assert!(!s.contains("recovery"));
    }

    // ---- parse / round-trip ---------------------------------------

    #[test]
    fn parse_round_trips_and_verifies() {
        let (gk, state, payload) = genesis_fixture(1);
        let inv = SignedInventory {
            signatures: vec![sign(&gk, "genesis", &payload)],
            payload,
        };
        let wire = serde_json::to_vec(&inv).unwrap();
        let parsed = SignedInventory::parse(&wire).unwrap();
        assert!(parsed.verify(&state).is_ok());
    }

    #[test]
    fn parse_rejects_duplicate_member_keys() {
        // Duplicate object keys inside members must be a hard parse
        // error (strict deserializer), not a silent last-write-wins.
        let wire = br#"{
            "payload": {
                "schema_version": 1,
                "canonical_encoding": "json/rfc8785",
                "mesh": "bus",
                "subnet": "192.0.2.0/24",
                "epoch": 1,
                "signed_at": "2026-06-03T00:00:00Z",
                "valid_until": "2026-06-10T00:00:00Z",
                "hub": ["beta"],
                "verify_keys": [],
                "members": [{"name": "a", "name": "b"}]
            },
            "signatures": []
        }"#;
        let err = SignedInventory::parse(wire).unwrap_err();
        assert!(err.to_string().contains("duplicate JSON object key"));
    }

    #[test]
    fn parse_rejects_unknown_payload_field() {
        // deny_unknown_fields: a smuggled field (e.g. a v2 field, or the
        // 1a `unsigned` marker) must fail to parse as a signed payload.
        let wire = br#"{
            "payload": {
                "schema_version": 1,
                "canonical_encoding": "json/rfc8785",
                "mesh": "bus",
                "subnet": "192.0.2.0/24",
                "epoch": 1,
                "signed_at": "2026-06-03T00:00:00Z",
                "valid_until": "2026-06-10T00:00:00Z",
                "hub": ["beta"],
                "verify_keys": [],
                "members": [],
                "unsigned": true
            },
            "signatures": []
        }"#;
        assert!(SignedInventory::parse(wire).is_err());
    }

    #[test]
    fn parse_tolerates_junk_in_unsigned_envelope_and_signature_bag() {
        // The strict/tolerant boundary: an unknown field at the envelope
        // level AND inside a signature entry must NOT fail the parse (they
        // are unsigned + carrier-malleable). Build a genuine inventory,
        // serialise it, then splice junk fields into the wire JSON; it must
        // still parse and still verify on the genuine signature.
        let (gk, state, payload) = genesis_fixture(1);
        let good = sign(&gk, "genesis", &payload);
        // Hand-roll wire JSON with an extra top-level field and an extra
        // field inside the signature object.
        let wire = format!(
            r#"{{
                "envelope_note": "carrier-added junk",
                "payload": {},
                "signatures": [
                    {{ "key_id": "genesis", "alg": "Ed25519", "sig": "{}", "carrier_junk": 42 }}
                ]
            }}"#,
            serde_json::to_string(&payload).unwrap(),
            good.sig,
        );
        let parsed = SignedInventory::parse(wire.as_bytes())
            .expect("unsigned envelope + signature junk must parse tolerantly");
        assert!(parsed.verify(&state).is_ok());
    }

    #[test]
    fn parse_drops_malformed_signature_entries() {
        // Carrier-appended malformed entries (empty object, missing fields,
        // wrong-typed sig) must be DROPPED, not DoS-fail the parse — the
        // genuine genesis signature still survives and verifies.
        let (gk, state, payload) = genesis_fixture(1);
        let good = sign(&gk, "genesis", &payload);
        let wire = format!(
            r#"{{
                "payload": {},
                "signatures": [
                    {{}},
                    {{ "key_id": "genesis" }},
                    {{ "key_id": "x", "alg": "Ed25519", "sig": 42 }},
                    {{ "key_id": "genesis", "alg": "Ed25519", "sig": "{}" }}
                ]
            }}"#,
            serde_json::to_string(&payload).unwrap(),
            good.sig,
        );
        let parsed = SignedInventory::parse(wire.as_bytes())
            .expect("malformed signature entries must be dropped, not fail the parse");
        // Only the one well-shaped entry survived parsing.
        assert_eq!(parsed.signatures.len(), 1);
        assert!(parsed.verify(&state).is_ok());
    }
}
