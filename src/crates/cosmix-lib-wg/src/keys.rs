//! WireGuard key material: Curve25519 keypairs, optional pre-shared keys,
//! base64 (de)serialisation matching `wg(8)` / `wg-quick` conventions.
//!
//! All three key types own their bytes (`[u8; 32]`) and zeroise on drop.
//! `Debug` is redacted on every secret-bearing type so accidental
//! `tracing::debug!("{:?}", key)` does not leak material into the journal.
//!
//! No `wg(8)` shell-out: keypair generation goes through `x25519-dalek`'s
//! `StaticSecret::random_from_rng` (which clamps the scalar per
//! Curve25519 requirements) and `PublicKey::from(&secret)`. PSKs are 32
//! uniformly-random bytes with no clamping — wireguard's PSK is mixed
//! into the BLAKE2s KDF, it is not a curve scalar.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use rand_core::{OsRng, RngCore};
use zeroize::{Zeroize, ZeroizeOnDrop};

/// Length of every WireGuard key (private, public, PSK) in bytes.
pub const KEY_LEN: usize = 32;

/// Length of the standard-base64 encoding of a 32-byte key (44 chars
/// including the single `=` pad), matching `wg(8)` output.
pub const KEY_B64_LEN: usize = 44;

/// Errors from key (de)serialisation.
#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error("invalid base64 in key: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("key has wrong length: expected {expected} bytes, got {got}")]
    Length { expected: usize, got: usize },
}

/// 32-byte Curve25519 private key (clamped scalar). Zeroised on drop;
/// `Debug` is redacted.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct WgPrivateKey([u8; KEY_LEN]);

/// 32-byte Curve25519 public key. Not secret — no zeroise, no redaction.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct WgPublicKey([u8; KEY_LEN]);

/// Optional 32-byte WireGuard pre-shared key. Zeroised on drop;
/// `Debug` is redacted.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct WgPresharedKey([u8; KEY_LEN]);

/// A freshly-generated Curve25519 keypair.
#[derive(Clone)]
pub struct WgKeyPair {
    pub private: WgPrivateKey,
    pub public: WgPublicKey,
}

/// Apply Curve25519 clamping in place. Matches `wg genkey`'s pre-encode
/// step (`src/genkey.c` in wireguard-tools): clear the low 3 bits of
/// byte 0, clear bit 255 of byte 31, set bit 254 of byte 31. x25519-dalek's
/// `StaticSecret::random_from_rng` stores raw bytes and clamps only on
/// scalar multiplication, so we clamp here for byte-level parity with
/// `wg(8)` output (the on-disk shape readers like `wg pubkey` expect).
fn clamp_scalar(bytes: &mut [u8; KEY_LEN]) {
    bytes[0] &= 0b1111_1000;
    bytes[31] &= 0b0111_1111;
    bytes[31] |= 0b0100_0000;
}

impl WgKeyPair {
    /// Generate a fresh Curve25519 keypair from the OS CSPRNG.
    ///
    /// Output is byte-identical in shape to what `wg genkey` writes:
    /// random 32 bytes with Curve25519 clamping applied before base64
    /// encoding.
    pub fn generate() -> Self {
        Self::generate_from_rng(&mut OsRng)
    }

    /// Generate a keypair from a caller-provided CSPRNG. Exposed for
    /// deterministic test vectors and future host-provisioning tooling.
    /// The RNG must be a real CSPRNG — there is no other safety check
    /// possible here.
    pub fn generate_from_rng<R: RngCore + rand_core::CryptoRng>(rng: &mut R) -> Self {
        let mut priv_bytes = [0u8; KEY_LEN];
        rng.fill_bytes(&mut priv_bytes);
        clamp_scalar(&mut priv_bytes);
        Self::from_private_bytes(priv_bytes)
    }

    /// Re-derive a keypair from a known private-key byte array. Useful for
    /// loading `secrets/<iface>.key` from disk during reconciliation.
    ///
    /// The input is clamped before use so a non-clamped input (e.g. from
    /// a non-`wg`-tools generator) produces the same public key whether
    /// re-encoded here or fed straight to the WireGuard kernel module.
    pub fn from_private_bytes(mut bytes: [u8; KEY_LEN]) -> Self {
        clamp_scalar(&mut bytes);
        let secret = x25519_dalek::StaticSecret::from(bytes);
        let public = x25519_dalek::PublicKey::from(&secret);
        WgKeyPair {
            private: WgPrivateKey(bytes),
            public: WgPublicKey(public.to_bytes()),
        }
    }
}

impl WgPrivateKey {
    /// Borrow the raw 32-byte scalar. Callers that serialise to disk
    /// should write into a `mode 0600` file owned by the wgd UID.
    pub fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }

    /// Standard-base64 encoding, matching `wg(8)` output.
    pub fn to_base64(&self) -> String {
        B64.encode(self.0)
    }

    /// Parse a standard-base64 key. The bytes are clamped on the way in
    /// so the type's invariant ("stored bytes are Curve25519-clamped")
    /// holds regardless of who generated the input — `wg genkey` already
    /// clamps, but non-wg generators may not.
    pub fn from_base64(s: &str) -> Result<Self, KeyError> {
        let mut bytes = decode_key(s)?;
        clamp_scalar(&mut bytes);
        Ok(Self(bytes))
    }

    /// Derive the public key from this private key.
    pub fn public_key(&self) -> WgPublicKey {
        let secret = x25519_dalek::StaticSecret::from(self.0);
        WgPublicKey(x25519_dalek::PublicKey::from(&secret).to_bytes())
    }
}

impl WgPublicKey {
    pub fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }

    /// Build a public key straight from its 32 raw bytes. A WireGuard public
    /// key IS a 32-byte X25519 point; a consumer that already holds the raw
    /// bytes (e.g. `cosmix_lib_mesh_trust::admission::select_wg_pubkeys`, which
    /// returns base64-decoded credential bytes so a diff keys on the canonical
    /// value rather than base64 spelling) should not have to re-encode and
    /// re-decode through base64 to obtain a typed key. Unlike a private key,
    /// a public key is not clamped — the 32 bytes are used verbatim.
    pub fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        Self(bytes)
    }

    pub fn to_base64(&self) -> String {
        B64.encode(self.0)
    }

    pub fn from_base64(s: &str) -> Result<Self, KeyError> {
        Ok(Self(decode_key(s)?))
    }
}

impl WgPresharedKey {
    /// Generate a fresh PSK from the OS CSPRNG.
    pub fn generate() -> Self {
        Self::generate_from_rng(&mut OsRng)
    }

    /// Generate a PSK from a caller-provided CSPRNG.
    pub fn generate_from_rng<R: RngCore + rand_core::CryptoRng>(rng: &mut R) -> Self {
        let mut bytes = [0u8; KEY_LEN];
        rng.fill_bytes(&mut bytes);
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }

    pub fn to_base64(&self) -> String {
        B64.encode(self.0)
    }

    pub fn from_base64(s: &str) -> Result<Self, KeyError> {
        Ok(Self(decode_key(s)?))
    }
}

/// Decode a 32-byte WireGuard key from its standard-base64 form. Pads are
/// required (matches `wg(8)`); a 43-char no-pad string is rejected as
/// length-mismatched after decode.
fn decode_key(s: &str) -> Result<[u8; KEY_LEN], KeyError> {
    let raw = B64.decode(s.trim())?;
    if raw.len() != KEY_LEN {
        return Err(KeyError::Length {
            expected: KEY_LEN,
            got: raw.len(),
        });
    }
    let mut out = [0u8; KEY_LEN];
    out.copy_from_slice(&raw);
    Ok(out)
}

// Redacted Debug for every secret-bearing type. The public key may be
// rendered verbatim because it is not secret — it is the peer identity
// printed in `wg show` output.

impl std::fmt::Debug for WgPrivateKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WgPrivateKey(***)")
    }
}

impl std::fmt::Debug for WgPresharedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WgPresharedKey(***)")
    }
}

impl std::fmt::Debug for WgPublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WgPublicKey({})", self.to_base64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_yields_clamped_private_key() {
        // Curve25519 clamping: low 3 bits of byte 0 cleared; bit 254 set
        // (== byte 31 has 0x40 set); bit 255 cleared (byte 31 < 0x80).
        let kp = WgKeyPair::generate();
        let b = kp.private.as_bytes();
        assert_eq!(b[0] & 0b0000_0111, 0, "low 3 bits must be clear");
        assert_eq!(
            b[31] & 0b1100_0000,
            0b0100_0000,
            "bit 254 set, bit 255 clear"
        );
    }

    #[test]
    fn public_derives_deterministically_from_private() {
        let kp = WgKeyPair::generate();
        let kp2 = WgKeyPair::from_private_bytes(*kp.private.as_bytes());
        assert_eq!(kp.public.as_bytes(), kp2.public.as_bytes());
        assert_eq!(kp.private.public_key().as_bytes(), kp.public.as_bytes());
    }

    #[test]
    fn base64_roundtrip_private() {
        let kp = WgKeyPair::generate();
        let b64 = kp.private.to_base64();
        assert_eq!(b64.len(), KEY_B64_LEN);
        let parsed = WgPrivateKey::from_base64(&b64).unwrap();
        assert_eq!(parsed.as_bytes(), kp.private.as_bytes());
    }

    #[test]
    fn base64_roundtrip_public() {
        let kp = WgKeyPair::generate();
        let b64 = kp.public.to_base64();
        assert_eq!(b64.len(), KEY_B64_LEN);
        let parsed = WgPublicKey::from_base64(&b64).unwrap();
        assert_eq!(parsed.as_bytes(), kp.public.as_bytes());
    }

    #[test]
    fn public_from_bytes_matches_from_base64_and_is_verbatim() {
        let kp = WgKeyPair::generate();
        // from_bytes(raw) and from_base64(encode(raw)) are the same key —
        // the round-trip a derive-from-inventory consumer would otherwise
        // have to pay to go from raw credential bytes to a typed key.
        let raw = *kp.public.as_bytes();
        let from_bytes = WgPublicKey::from_bytes(raw);
        assert_eq!(from_bytes.as_bytes(), kp.public.as_bytes());
        assert_eq!(
            from_bytes,
            WgPublicKey::from_base64(&kp.public.to_base64()).unwrap()
        );
        // A public key is NOT clamped (unlike a private key): the 32 bytes
        // are stored verbatim, even a non-canonical pattern.
        let odd = [0xffu8; KEY_LEN];
        assert_eq!(WgPublicKey::from_bytes(odd).as_bytes(), &odd);
    }

    #[test]
    fn base64_roundtrip_psk() {
        let psk = WgPresharedKey::generate();
        let b64 = psk.to_base64();
        assert_eq!(b64.len(), KEY_B64_LEN);
        let parsed = WgPresharedKey::from_base64(&b64).unwrap();
        assert_eq!(parsed.as_bytes(), psk.as_bytes());
    }

    #[test]
    fn from_base64_rejects_wrong_length() {
        // 31 bytes after decode (one short).
        let short = B64.encode([0u8; 31]);
        match WgPrivateKey::from_base64(&short) {
            Err(KeyError::Length { expected, got }) => {
                assert_eq!(expected, KEY_LEN);
                assert_eq!(got, 31);
            }
            other => panic!("expected Length error, got {other:?}"),
        }
    }

    #[test]
    fn from_base64_rejects_non_base64() {
        assert!(matches!(
            WgPrivateKey::from_base64("not base64 !!!"),
            Err(KeyError::Base64(_))
        ));
    }

    #[test]
    fn from_base64_trims_whitespace() {
        let kp = WgKeyPair::generate();
        let raw = kp.public.to_base64();
        let padded = format!(" \n{raw}\n");
        let parsed = WgPublicKey::from_base64(&padded).unwrap();
        assert_eq!(parsed.as_bytes(), kp.public.as_bytes());
    }

    #[test]
    fn debug_redacts_secret_bytes() {
        let kp = WgKeyPair::generate();
        let priv_b64 = kp.private.to_base64();
        let priv_dbg = format!("{:?}", kp.private);
        assert!(
            !priv_dbg.contains(&priv_b64),
            "private base64 leaked: {priv_dbg}"
        );
        assert_eq!(priv_dbg, "WgPrivateKey(***)");

        let psk = WgPresharedKey::generate();
        let psk_b64 = psk.to_base64();
        let psk_dbg = format!("{:?}", psk);
        assert!(!psk_dbg.contains(&psk_b64), "psk base64 leaked: {psk_dbg}");
        assert_eq!(psk_dbg, "WgPresharedKey(***)");
    }

    #[test]
    fn debug_public_includes_base64() {
        // Public keys are not secret; printing them in debug output is
        // expected (it's what `wg show` does).
        let kp = WgKeyPair::generate();
        let b64 = kp.public.to_base64();
        assert_eq!(format!("{:?}", kp.public), format!("WgPublicKey({b64})"));
    }

    #[test]
    fn psk_is_uniformly_random_not_clamped() {
        // A PSK is NOT a Curve25519 scalar; clamping it would reduce its
        // entropy. The probabilistic check below would fail with vanishing
        // probability if any clamping had been applied — we generate many
        // PSKs and assert that we see at least one byte 0 with the low 3
        // bits non-zero (which clamping would have cleared).
        let mut saw_unclamped = false;
        for _ in 0..256 {
            let psk = WgPresharedKey::generate();
            if psk.as_bytes()[0] & 0b0000_0111 != 0 {
                saw_unclamped = true;
                break;
            }
        }
        assert!(saw_unclamped, "PSK appears to be Curve25519-clamped");
    }

    #[test]
    fn from_base64_clamps_non_canonical_input() {
        // Construct an unclamped 32-byte input (low 3 bits set in byte 0,
        // bit 255 set in byte 31). Parse it as a private key; the stored
        // bytes must come out clamped.
        let mut raw = [0xffu8; KEY_LEN];
        let b64 = B64.encode(raw);
        let key = WgPrivateKey::from_base64(&b64).unwrap();
        clamp_scalar(&mut raw);
        assert_eq!(key.as_bytes(), &raw);
        assert_eq!(key.as_bytes()[0] & 0b0000_0111, 0);
        assert_eq!(key.as_bytes()[31] & 0b1100_0000, 0b0100_0000);
    }

    #[test]
    fn from_private_bytes_clamps_non_canonical_input() {
        // Same property for the in-memory path used by reconciler reload.
        let mut raw = [0xffu8; KEY_LEN];
        let kp = WgKeyPair::from_private_bytes(raw);
        clamp_scalar(&mut raw);
        assert_eq!(kp.private.as_bytes(), &raw);
    }

    #[test]
    fn public_key_stable_across_clamping_normalisation() {
        // Two callers feed the same scalar in clamped and unclamped
        // shape; both should yield the same public key (the WG kernel
        // module would clamp internally, so this matches kernel
        // semantics).
        let mut clamped = [0u8; KEY_LEN];
        OsRng.fill_bytes(&mut clamped);
        let mut unclamped = clamped;
        clamp_scalar(&mut clamped);
        // Set bits clamping would clear so `unclamped` is genuinely
        // pre-clamp shape.
        unclamped[0] |= 0b0000_0111;
        unclamped[31] |= 0b1000_0000;
        unclamped[31] &= 0b1011_1111;

        let a = WgKeyPair::from_private_bytes(clamped);
        let b = WgKeyPair::from_private_bytes(unclamped);
        assert_eq!(a.public.as_bytes(), b.public.as_bytes());
    }

    #[test]
    fn distinct_generations_differ() {
        let a = WgKeyPair::generate();
        let b = WgKeyPair::generate();
        assert_ne!(a.private.as_bytes(), b.private.as_bytes());
        assert_ne!(a.public.as_bytes(), b.public.as_bytes());
    }
}
