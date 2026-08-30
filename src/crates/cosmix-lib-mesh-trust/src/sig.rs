//! Ed25519 signature verification + key selection (plan §6 step 3).
//!
//! Pure crypto + lookup. No IO, no async. The HTTPS verifier
//! (plan §6) wires this into its pipeline; the cosmix-feature
//! combinator does not — it operates on a `PeerIdentity` that has
//! already been signature-verified by the verifier.

use chrono::{DateTime, Utc};
use ed25519_dalek::{
    SECRET_KEY_LENGTH, SIGNATURE_LENGTH, Signature, Signer, SigningKey, VerifyingKey,
};

use crate::caps::TrustedMesh;

pub const PUBLIC_KEY_LENGTH: usize = 32;
/// Length of an Ed25519 private key (seed), in bytes.
pub const PRIVATE_KEY_LENGTH: usize = SECRET_KEY_LENGTH;

/// Errors from signature verification.
#[derive(Debug, thiserror::Error)]
pub enum SigError {
    #[error(
        "public key has wrong length: expected {expected}, got {got}",
        expected = PUBLIC_KEY_LENGTH
    )]
    InvalidKeyLength { got: usize },

    #[error(
        "signature has wrong length: expected {expected}, got {got}",
        expected = SIGNATURE_LENGTH
    )]
    InvalidSigLength { got: usize },

    #[error("public key is not a valid Ed25519 point")]
    InvalidKey,

    #[error("signature verification failed")]
    VerifyFailed,

    #[error(
        "private key has wrong length: expected {expected}, got {got}",
        expected = PRIVATE_KEY_LENGTH
    )]
    InvalidPrivateKeyLength { got: usize },
}

/// Errors from key selection (plan §6 step 3).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum KeySelectError {
    #[error("trusted mesh is disabled")]
    Disabled,

    #[error("trust is stale: last_verified_at + max_pubkey_age_secs < now")]
    StaleTrust,

    #[error("no key matches caller_selector {caller_selector:?}")]
    SelectorMismatch { caller_selector: String },

    #[error("previous key matched caller_selector but is past its rotation grace deadline")]
    PreviousKeyExpired,
}

/// Strict Ed25519 verification over `message` (which MUST be the
/// envelope's v1 canonical bytes — never the wire bytes).
///
/// Uses `verify_strict` to reject signatures with non-canonical
/// scalars (malleability defense) and mixed-form public keys.
pub fn verify_ed25519(public_key: &[u8], signature: &[u8], message: &[u8]) -> Result<(), SigError> {
    if public_key.len() != PUBLIC_KEY_LENGTH {
        return Err(SigError::InvalidKeyLength {
            got: public_key.len(),
        });
    }
    if signature.len() != SIGNATURE_LENGTH {
        return Err(SigError::InvalidSigLength {
            got: signature.len(),
        });
    }
    let pk_arr: &[u8; PUBLIC_KEY_LENGTH] = public_key.try_into().unwrap();
    let sig_arr: &[u8; SIGNATURE_LENGTH] = signature.try_into().unwrap();

    let vk = VerifyingKey::from_bytes(pk_arr).map_err(|_| SigError::InvalidKey)?;
    let sig = Signature::from_bytes(sig_arr);
    vk.verify_strict(message, &sig)
        .map_err(|_| SigError::VerifyFailed)
}

/// The signing mirror of [`verify_ed25519`]: produce a detached Ed25519
/// signature over `message` with a 32-byte private key (seed). Pure — no RNG,
/// no IO. This is the PROVER primitive `verify_ed25519` checks: a node signs
/// the §9a admission transcript (see [`crate::admission::sign_admission_transcript`])
/// with its `kind:"d2"` key, and the broker verifies that same byte string. Key
/// GENERATION is deliberately NOT here — it needs the OS RNG, which belongs in
/// the (IO-permitted) provisioner, keeping this crate's "pure crypto" contract.
pub fn sign_ed25519(
    private_key: &[u8],
    message: &[u8],
) -> Result<[u8; SIGNATURE_LENGTH], SigError> {
    let seed: &[u8; PRIVATE_KEY_LENGTH] =
        private_key
            .try_into()
            .map_err(|_| SigError::InvalidPrivateKeyLength {
                got: private_key.len(),
            })?;
    Ok(SigningKey::from_bytes(seed).sign(message).to_bytes())
}

/// Plan §6 step 3 key selection: given the trusted-mesh record and
/// the envelope's `caller_selector`, return the public key to verify
/// the envelope against. Honours the rotation-grace window for the
/// previous key.
///
/// Also enforces the plan §4 freshness gate (mesh `enabled` +
/// `last_verified_at + max_pubkey_age_secs >= now`) — same checks
/// the resolver does, repeated here so a service that runs sig
/// verify without first calling the resolver still gets the gate.
pub fn select_pubkey<'a>(
    trusted: &'a TrustedMesh,
    caller_selector: &str,
    now: DateTime<Utc>,
) -> Result<&'a [u8], KeySelectError> {
    if !trusted.enabled {
        return Err(KeySelectError::Disabled);
    }

    // Same overflow-safe arithmetic as caps::resolve_cross_mesh_caps:
    // operator-supplied u64 → checked Duration → checked timestamp add.
    let clamped = trusted.max_pubkey_age_secs.min(i64::MAX as u64) as i64;
    let Some(max_age) = chrono::Duration::try_seconds(clamped) else {
        return Err(KeySelectError::StaleTrust);
    };
    match trusted.last_verified_at.checked_add_signed(max_age) {
        Some(t) if now <= t => {}
        _ => return Err(KeySelectError::StaleTrust),
    }

    if trusted.current_selector == caller_selector {
        return Ok(&trusted.current_pubkey);
    }
    if let (Some(prev_sel), Some(prev_key)) = (&trusted.previous_selector, &trusted.previous_pubkey)
        && prev_sel == caller_selector
    {
        // Plan §4: previous_pubkey_valid_until is mandatory whenever
        // previous_pubkey is set; absence is treated as expired.
        match trusted.previous_pubkey_valid_until {
            Some(deadline) if now <= deadline => return Ok(prev_key),
            _ => return Err(KeySelectError::PreviousKeyExpired),
        }
    }
    Err(KeySelectError::SelectorMismatch {
        caller_selector: caller_selector.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone as _};
    use ed25519_dalek::{Signer, SigningKey};
    use rand::rngs::OsRng;

    fn fresh_keypair() -> (SigningKey, [u8; 32]) {
        let sk = SigningKey::generate(&mut OsRng);
        let pk = sk.verifying_key().to_bytes();
        (sk, pk)
    }

    fn trusted_with(pk: [u8; 32], selector: &str, now: DateTime<Utc>) -> TrustedMesh {
        TrustedMesh {
            mesh_fqdn: "example.com".into(),
            current_pubkey: pk.to_vec(),
            current_selector: selector.into(),
            previous_pubkey: None,
            previous_selector: None,
            previous_pubkey_valid_until: None,
            last_verified_at: now - Duration::hours(1),
            max_pubkey_age_secs: 7 * 86400,
            enabled: true,
        }
    }

    #[test]
    fn verify_accepts_valid_signature() {
        let (sk, pk) = fresh_keypair();
        let msg = b"canonical-bytes-for-an-envelope";
        let sig = sk.sign(msg);
        assert!(verify_ed25519(&pk, sig.to_bytes().as_ref(), msg).is_ok());
    }

    #[test]
    fn verify_rejects_wrong_key() {
        let (sk, _) = fresh_keypair();
        let (_, other_pk) = fresh_keypair();
        let msg = b"canonical-bytes";
        let sig = sk.sign(msg);
        assert!(matches!(
            verify_ed25519(&other_pk, sig.to_bytes().as_ref(), msg),
            Err(SigError::VerifyFailed)
        ));
    }

    #[test]
    fn verify_rejects_tampered_message() {
        let (sk, pk) = fresh_keypair();
        let sig = sk.sign(b"original");
        assert!(matches!(
            verify_ed25519(&pk, sig.to_bytes().as_ref(), b"tampered"),
            Err(SigError::VerifyFailed)
        ));
    }

    #[test]
    fn verify_rejects_bad_lengths() {
        assert!(matches!(
            verify_ed25519(&[0u8; 31], &[0u8; 64], b""),
            Err(SigError::InvalidKeyLength { got: 31 })
        ));
        assert!(matches!(
            verify_ed25519(&[0u8; 32], &[0u8; 63], b""),
            Err(SigError::InvalidSigLength { got: 63 })
        ));
    }

    #[test]
    fn sign_then_verify_round_trips() {
        // sign_ed25519 (prover) and verify_ed25519 (broker) agree byte-for-byte.
        let sk = SigningKey::generate(&mut OsRng);
        let pk = sk.verifying_key().to_bytes();
        let seed = sk.to_bytes(); // 32-byte private seed
        let msg = b"canonical admission transcript bytes";
        let sig = sign_ed25519(&seed, msg).unwrap();
        assert!(verify_ed25519(&pk, &sig, msg).is_ok());
        // A different message does not verify against that signature.
        assert!(matches!(
            verify_ed25519(&pk, &sig, b"other"),
            Err(SigError::VerifyFailed)
        ));
    }

    #[test]
    fn sign_rejects_bad_private_key_length() {
        assert!(matches!(
            sign_ed25519(&[0u8; 31], b"msg"),
            Err(SigError::InvalidPrivateKeyLength { got: 31 })
        ));
        assert!(matches!(
            sign_ed25519(&[0u8; 64], b"msg"),
            Err(SigError::InvalidPrivateKeyLength { got: 64 })
        ));
    }

    #[test]
    fn select_pubkey_picks_current() {
        let now = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();
        let (_, pk) = fresh_keypair();
        let trusted = trusted_with(pk, "2026q3", now);
        let selected = select_pubkey(&trusted, "2026q3", now).unwrap();
        assert_eq!(selected, &pk[..]);
    }

    #[test]
    fn select_pubkey_falls_back_to_previous_within_grace() {
        let now = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();
        let (_, current_pk) = fresh_keypair();
        let (_, prev_pk) = fresh_keypair();
        let mut trusted = trusted_with(current_pk, "2026q4", now);
        trusted.previous_pubkey = Some(prev_pk.to_vec());
        trusted.previous_selector = Some("2026q3".into());
        trusted.previous_pubkey_valid_until = Some(now + Duration::days(7));

        let selected = select_pubkey(&trusted, "2026q3", now).unwrap();
        assert_eq!(selected, &prev_pk[..]);
    }

    #[test]
    fn select_pubkey_rejects_previous_past_grace() {
        let now = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();
        let (_, current_pk) = fresh_keypair();
        let (_, prev_pk) = fresh_keypair();
        let mut trusted = trusted_with(current_pk, "2026q4", now);
        trusted.previous_pubkey = Some(prev_pk.to_vec());
        trusted.previous_selector = Some("2026q3".into());
        trusted.previous_pubkey_valid_until = Some(now - Duration::seconds(1));

        assert_eq!(
            select_pubkey(&trusted, "2026q3", now),
            Err(KeySelectError::PreviousKeyExpired)
        );
    }

    #[test]
    fn select_pubkey_rejects_previous_without_deadline() {
        let now = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();
        let (_, current_pk) = fresh_keypair();
        let (_, prev_pk) = fresh_keypair();
        let mut trusted = trusted_with(current_pk, "2026q4", now);
        trusted.previous_pubkey = Some(prev_pk.to_vec());
        trusted.previous_selector = Some("2026q3".into());
        trusted.previous_pubkey_valid_until = None; // schema violation: mandatory whenever previous_pubkey is set

        assert_eq!(
            select_pubkey(&trusted, "2026q3", now),
            Err(KeySelectError::PreviousKeyExpired)
        );
    }

    #[test]
    fn select_pubkey_rejects_unknown_selector() {
        let now = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();
        let (_, pk) = fresh_keypair();
        let trusted = trusted_with(pk, "2026q3", now);
        assert!(matches!(
            select_pubkey(&trusted, "wrong", now),
            Err(KeySelectError::SelectorMismatch { .. })
        ));
    }

    #[test]
    fn select_pubkey_rejects_disabled_mesh() {
        let now = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();
        let (_, pk) = fresh_keypair();
        let mut trusted = trusted_with(pk, "2026q3", now);
        trusted.enabled = false;
        assert_eq!(
            select_pubkey(&trusted, "2026q3", now),
            Err(KeySelectError::Disabled)
        );
    }

    #[test]
    fn select_pubkey_rejects_stale_trust() {
        let now = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();
        let (_, pk) = fresh_keypair();
        let mut trusted = trusted_with(pk, "2026q3", now);
        trusted.last_verified_at = now - Duration::days(30); // past 7-day default
        assert_eq!(
            select_pubkey(&trusted, "2026q3", now),
            Err(KeySelectError::StaleTrust)
        );
    }

    #[test]
    fn end_to_end_select_and_verify() {
        // Produce a real signature, look up the key via the same
        // selector the producer claimed, and verify.
        let now = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();
        let sk = SigningKey::generate(&mut OsRng);
        let pk = sk.verifying_key().to_bytes();
        let trusted = trusted_with(pk, "2026q3", now);

        let msg = b"canonical envelope bytes";
        let sig = sk.sign(msg);

        let selected = select_pubkey(&trusted, "2026q3", now).unwrap();
        assert!(verify_ed25519(selected, sig.to_bytes().as_ref(), msg).is_ok());
    }
}
