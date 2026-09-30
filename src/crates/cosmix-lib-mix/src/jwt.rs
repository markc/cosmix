//! Generic RS256 JWT signing. Claims/header semantics belong to the caller;
//! the core enforces JSON object shape, algorithm, input bounds and RSA key
//! validity. Static failures never echo claims, headers or private key material.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use rustls_pemfile::Item;
use serde_json::{Map, Value};
use std::io::Cursor;

use crate::error::{MixError, MixResult};

const MAX_INPUT_LEN: usize = 256 * 1024; // 256 KB safety bound

/// Parses a JSON string into a JSON Object, enforcing size bounds and masking errors.
fn parse_json_object(input: &str, err_code: &str) -> MixResult<Map<String, Value>> {
    if input.len() > MAX_INPUT_LEN {
        return Err(MixError::structured(
            err_code,
            "Input exceeds maximum allowed size",
        ));
    }
    let parsed: Value = serde_json::from_str(input)
        .map_err(|_| MixError::structured(err_code, "Malformed JSON input"))?;

    match parsed {
        Value::Object(obj) => Ok(obj),
        _ => Err(MixError::structured(err_code, "JSON must be an object")),
    }
}

/// Signs a JWT using RS256 with the given JSON claims and PEM-encoded private key.
pub(crate) fn sign_rs256(
    claims_json: &str,
    private_pem: &str,
    header_json: Option<&str>,
) -> MixResult<String> {
    // Parse claims
    let claims = parse_json_object(claims_json, "JWT_CLAIMS_INVALID")?;

    // Parse and validate header
    let mut header = match header_json {
        Some(h) => parse_json_object(h, "JWT_HEADER_INVALID")?,
        None => {
            let mut m = Map::new();
            m.insert("typ".to_string(), Value::String("JWT".to_string()));
            m
        }
    };

    // Enforce RS256 algorithm
    if let Some(alg) = header.get("alg") {
        if alg != "RS256" {
            return Err(MixError::structured(
                "JWT_HEADER_INVALID",
                "Algorithm must be exactly RS256",
            ));
        }
    } else {
        header.insert("alg".to_string(), Value::String("RS256".to_string()));
    }

    // Parse private key
    if private_pem.len() > MAX_INPUT_LEN {
        return Err(MixError::structured(
            "JWT_KEY_INVALID",
            "PEM exceeds max size",
        ));
    }

    let mut pem_cursor = Cursor::new(private_pem.as_bytes());
    let item = rustls_pemfile::read_one(&mut pem_cursor)
        .map_err(|_| MixError::structured("JWT_KEY_INVALID", "Failed to read PEM"))?
        .ok_or_else(|| MixError::structured("JWT_KEY_INVALID", "No PEM item found"))?;

    let key_pair = match item {
        Item::Pkcs8Key(key) => RsaKeyPair::from_pkcs8(key.secret_pkcs8_der()).map_err(|_| {
            MixError::structured("JWT_KEY_INVALID", "Invalid PKCS8 RSA key structure")
        })?,
        Item::Pkcs1Key(key) => RsaKeyPair::from_der(key.secret_pkcs1_der()).map_err(|_| {
            MixError::structured("JWT_KEY_INVALID", "Invalid PKCS1 RSA key structure")
        })?,
        _ => {
            return Err(MixError::structured(
                "JWT_KEY_INVALID",
                "Unsupported PEM type (expected PKCS8 or PKCS1 RSA)",
            ));
        }
    };

    // Serialize and Base64 URL-Safe encode
    let header_str = serde_json::to_string(&header)
        .map_err(|_| MixError::structured("JWT_HEADER_INVALID", "Failed to serialize header"))?;
    let claims_str = serde_json::to_string(&claims)
        .map_err(|_| MixError::structured("JWT_CLAIMS_INVALID", "Failed to serialize claims"))?;

    let header_b64 = URL_SAFE_NO_PAD.encode(header_str.as_bytes());
    let claims_b64 = URL_SAFE_NO_PAD.encode(claims_str.as_bytes());

    let signing_input = format!("{}.{}", header_b64, claims_b64);

    // Sign payload
    let rng = SystemRandom::new();
    let mut signature = vec![0; key_pair.public().modulus_len()];
    key_pair
        .sign(
            &RSA_PKCS1_SHA256,
            &rng,
            signing_input.as_bytes(),
            &mut signature,
        )
        .map_err(|_| MixError::structured("JWT_SIGN_FAILED", "RSA signing failed"))?;

    let sig_b64 = URL_SAFE_NO_PAD.encode(&signature);

    Ok(format!("{}.{}", signing_input, sig_b64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{KeyPair, RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};
    use std::fs;

    fn get_test_key() -> String {
        fs::read_to_string("tests/fixtures/jwt-rsa-test.pem")
            .expect("Missing test fixture tests/fixtures/jwt-rsa-test.pem")
    }

    #[test]
    fn oversized_inputs_are_refused_without_echoing_them() {
        let sentinel = "PRIVATE_INPUT_SENTINEL";
        let oversized = sentinel.repeat(MAX_INPUT_LEN / sentinel.len() + 1);
        for (claims, key, header, code) in [
            (oversized.as_str(), "", None, "JWT_CLAIMS_INVALID"),
            ("{}", "", Some(oversized.as_str()), "JWT_HEADER_INVALID"),
            ("{}", oversized.as_str(), None, "JWT_KEY_INVALID"),
        ] {
            let err = sign_rs256(claims, key, header).unwrap_err();
            let rendered = format!("{err:?}");
            assert!(rendered.contains(code), "wrong refusal: {rendered}");
            assert!(!rendered.contains(sentinel), "input leaked: {rendered}");
        }
    }

    #[test]
    fn test_sign_and_verify_valid() {
        let pem = get_test_key();
        let claims = r#"{"sub":"1234567890","name":"Mixer","iat":1516239022}"#;

        let token = sign_rs256(claims, &pem, None).expect("Failed to sign token");
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3);

        // Verify exact round-trip format via base64url-no-padding decoding
        let header_dec = URL_SAFE_NO_PAD.decode(parts[0]).unwrap();
        let header_json: Value = serde_json::from_slice(&header_dec).unwrap();
        assert_eq!(header_json["alg"], "RS256");
        assert_eq!(header_json["typ"], "JWT");

        let claims_dec = URL_SAFE_NO_PAD.decode(parts[1]).unwrap();
        let claims_json: Value = serde_json::from_slice(&claims_dec).unwrap();
        assert_eq!(claims_json["sub"], "1234567890");

        // Verify output via ring public key verifier
        let mut cursor = Cursor::new(pem.as_bytes());
        let item = rustls_pemfile::read_one(&mut cursor).unwrap().unwrap();
        let key_pair = match item {
            Item::Pkcs8Key(k) => RsaKeyPair::from_pkcs8(k.secret_pkcs8_der()).unwrap(),
            Item::Pkcs1Key(k) => RsaKeyPair::from_der(k.secret_pkcs1_der()).unwrap(),
            _ => panic!("Unexpected key type"),
        };

        let public_key_der = key_pair.public_key().as_ref();
        let unparsed_pk = UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, public_key_der);

        let signing_input = format!("{}.{}", parts[0], parts[1]);
        let sig_bytes = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();

        assert!(
            unparsed_pk
                .verify(signing_input.as_bytes(), &sig_bytes)
                .is_ok()
        );
    }

    #[test]
    fn test_tampering_refusal() {
        let pem = get_test_key();
        let claims = r#"{"sub":"user1"}"#;
        let token = sign_rs256(claims, &pem, None).unwrap();

        let mut parts: Vec<&str> = token.split('.').collect();

        // Tamper with payload
        let mut tampered_claims = URL_SAFE_NO_PAD.decode(parts[1]).unwrap();
        if !tampered_claims.is_empty() {
            tampered_claims[0] ^= 0x01; // flip a bit
        }
        let tampered_b64 = URL_SAFE_NO_PAD.encode(&tampered_claims);
        parts[1] = &tampered_b64;

        let signing_input = format!("{}.{}", parts[0], parts[1]);
        let sig_bytes = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();

        let mut cursor = Cursor::new(pem.as_bytes());
        let item = rustls_pemfile::read_one(&mut cursor).unwrap().unwrap();
        let key_pair = match item {
            Item::Pkcs8Key(k) => RsaKeyPair::from_pkcs8(k.secret_pkcs8_der()).unwrap(),
            Item::Pkcs1Key(k) => RsaKeyPair::from_der(k.secret_pkcs1_der()).unwrap(),
            _ => panic!("Unexpected key type"),
        };

        let unparsed_pk =
            UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, key_pair.public_key().as_ref());
        assert!(
            unparsed_pk
                .verify(signing_input.as_bytes(), &sig_bytes)
                .is_err()
        );
    }

    #[test]
    fn test_invalid_claims_format() {
        let pem = get_test_key();

        // JSON array instead of object
        let err = sign_rs256(r#"[1, 2, 3]"#, &pem, None).unwrap_err();
        assert!(format!("{:?}", err).contains("JWT_CLAIMS_INVALID"));

        // Malformed JSON (safe error without leaking input)
        let err = sign_rs256(r#"{"sub":"user1""#, &pem, None).unwrap_err();
        assert!(format!("{:?}", err).contains("JWT_CLAIMS_INVALID"));
        assert!(!format!("{:?}", err).contains("user1"));
    }

    #[test]
    fn test_invalid_header_algorithm() {
        let pem = get_test_key();
        let claims = r#"{"sub":"user1"}"#;

        // Disallow wrong algorithms implicitly
        let header = r#"{"alg":"HS256","typ":"JWT"}"#;
        let err = sign_rs256(claims, &pem, Some(header)).unwrap_err();
        assert!(format!("{:?}", err).contains("JWT_HEADER_INVALID"));
    }

    #[test]
    fn test_custom_header_with_kid() {
        let pem = get_test_key();
        let claims = r#"{"sub":"user1"}"#;
        // Missing alg, module should automatically insert RS256
        let header = r#"{"kid":"key-1","typ":"JWT"}"#;

        let token = sign_rs256(claims, &pem, Some(header)).unwrap();
        let parts: Vec<&str> = token.split('.').collect();

        let header_dec = URL_SAFE_NO_PAD.decode(parts[0]).unwrap();
        let header_json: Value = serde_json::from_slice(&header_dec).unwrap();

        assert_eq!(header_json["alg"], "RS256");
        assert_eq!(header_json["kid"], "key-1");
    }

    #[test]
    fn test_invalid_non_rsa_key_safe_error() {
        let claims = r#"{"sub":"user1"}"#;
        let bad_pem = "-----BEGIN PRIVATE KEY-----\nMIICdwIB\n-----END PRIVATE KEY-----";

        let err = sign_rs256(claims, bad_pem, None).unwrap_err();
        assert!(format!("{:?}", err).contains("JWT_KEY_INVALID"));

        // Ensure no bytes from the bad key appear in the error string
        assert!(!format!("{:?}", err).contains("MIIC"));
    }
}
