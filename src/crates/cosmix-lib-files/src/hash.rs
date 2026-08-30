//! BLAKE3 content hashing — the #6 hash-verify key and #7 reconcile signal.
//! Matches `cosmix-mds` blob hashing so a corpus hash and a CAS hash are the same
//! function.

/// Lowercase 64-char hex BLAKE3 of `bytes`.
pub fn content_hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_empty_vector() {
        // BLAKE3 of the empty input is a fixed, well-known digest.
        assert_eq!(
            content_hash(b""),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
    }

    #[test]
    fn stable_and_distinct() {
        assert_eq!(content_hash(b"hello"), content_hash(b"hello"));
        assert_ne!(content_hash(b"hello"), content_hash(b"hellp"));
        assert_eq!(content_hash(b"hello").len(), 64);
    }
}
