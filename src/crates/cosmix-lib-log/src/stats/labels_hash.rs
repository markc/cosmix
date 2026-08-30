//! Canonical `labels_hash` encoding (plan §4.1).
//!
//! The label set is canonicalised into a length-prefixed byte string
//! and hashed with FxHash. The result is a 16-hex-char digest that
//! appears in three places:
//!
//! - JSONL records under `Restricted` families (`labels_hash` field
//!   replacing `labels`).
//! - Cardinality-cap `warn` events (one per metric per hour) so an
//!   operator can correlate disk records with warning entries.
//! - `SeriesLabels::Hash(..)` in snapshot responses that don't carry
//!   the per-host raw-labels capability.
//!
//! # Encoding (frozen at v1)
//!
//! ```text
//! n = pair_count
//! out = u32_le(n)
//! for (k, v) in sorted_by_key(label-set):
//!     out ||= u32_le(len_bytes(k)) || utf8_bytes(k)
//!     out ||= u32_le(len_bytes(v)) || utf8_bytes(v)
//! labels_hash = lower_hex(FxHash::hash(&out))[..16]
//! ```
//!
//! The sort is lexicographic byte-order on keys; `len_bytes` is the
//! UTF-8 byte length, not character count. Length prefixes make the
//! encoding *injective* on `BTreeMap<String, String>` — distinct
//! label-sets always produce distinct pre-hash byte strings, so any
//! collision in the 64-bit hash output is a genuine hash collision
//! (vanishingly rare), not an aliasing bug from boundary ambiguity.
//!
//! # Why FxHash, not a cryptographic hash
//!
//! `labels_hash` is *not* a cryptographic redaction — it's a stable
//! identifier that ships in the same byte form across surfaces. An
//! attacker holding the JSONL stream can already pre-compute the
//! hash of any guessed label-set, so collision-resistance matters
//! more than pre-image resistance. FxHash is fast (the writer path
//! hashes every line under `Restricted` families), deterministic
//! across processes (so cross-process aggregators group correctly),
//! and matches the variant the recorder uses internally for
//! cardinality-cap dedup.

use rustc_hash::FxHasher;
use std::collections::BTreeMap;
use std::hash::Hasher;

/// Build the canonical pre-hash byte string for a label set. Public
/// for golden-vector tests that pin the wire shape; production
/// callers should go through [`labels_hash`].
pub fn labels_hash_bytes(labels: &BTreeMap<String, String>) -> Vec<u8> {
    let n = u32::try_from(labels.len()).expect("label-set size fits in u32 (cap = 4096)");
    // Pre-size by counting key+value bytes; saves reallocations on the
    // common per-record path. 4 bytes pair count + per-pair (4 + key + 4 + value).
    let pair_bytes: usize = labels.iter().map(|(k, v)| 8 + k.len() + v.len()).sum();
    let mut out = Vec::with_capacity(4 + pair_bytes);
    out.extend_from_slice(&n.to_le_bytes());
    // BTreeMap iterates in sorted key order; that *is* the canonical
    // sort. Explicit `sort_by_key` would be redundant.
    for (k, v) in labels {
        let k_len = u32::try_from(k.len()).expect("label key length fits in u32");
        let v_len = u32::try_from(v.len()).expect("label value length fits in u32");
        out.extend_from_slice(&k_len.to_le_bytes());
        out.extend_from_slice(k.as_bytes());
        out.extend_from_slice(&v_len.to_le_bytes());
        out.extend_from_slice(v.as_bytes());
    }
    out
}

/// Compute the 16-hex-char canonical `labels_hash` for a label set.
///
/// Always lowercase, always exactly 16 hex characters (the 64-bit
/// FxHash output formatted with leading zeros), so the JSONL line
/// schema can pin field width without worrying about variable
/// rendering.
pub fn labels_hash(labels: &BTreeMap<String, String>) -> String {
    let bytes = labels_hash_bytes(labels);
    let mut hasher = FxHasher::default();
    hasher.write(&bytes);
    format!("{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn empty_label_set_hashes_deterministically() {
        // The empty label set is a valid input (`metrics::counter!`
        // with no key=value pairs lands here); the encoding still
        // emits the 4-byte pair count of 0.
        let hash = labels_hash(&map(&[]));
        assert_eq!(hash.len(), 16);
        // Stability: the same input always produces the same hash.
        assert_eq!(hash, labels_hash(&map(&[])));
    }

    #[test]
    fn output_is_always_sixteen_lowercase_hex_chars() {
        for input in [
            map(&[]),
            map(&[("k", "v")]),
            map(&[("a", "1"), ("b", "2"), ("c", "3")]),
        ] {
            let hash = labels_hash(&input);
            assert_eq!(hash.len(), 16, "expected 16-char hash for {input:?}");
            assert!(
                hash.chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "hash {hash} must be lowercase hex only"
            );
        }
    }

    #[test]
    fn distinct_label_sets_have_distinct_pre_hash_bytes() {
        // The plan's injectivity guarantee is on the PRE-hash bytes
        // (the hash itself can collide as any 64-bit hash can).
        // These four sets are pairwise distinct in canonical encoding
        // — anti-regression for any future re-encoding that drops a
        // length prefix.
        let cases = [
            // Single pair vs. two pairs (different pair counts).
            map(&[("a", "b")]),
            map(&[("a", "bc")]),
            // Splitting ambiguity that a key=value comma format would
            // collide: {"a=b": "c"} vs {"a": "b=c"} vs {"a,b": "c"}.
            map(&[("a=b", "c")]),
            map(&[("a", "b=c")]),
            map(&[("a,b", "c")]),
        ];
        let mut seen: Vec<Vec<u8>> = Vec::new();
        for c in cases {
            let bytes = labels_hash_bytes(&c);
            assert!(
                !seen.contains(&bytes),
                "pre-hash bytes collided across distinct inputs"
            );
            seen.push(bytes);
        }
    }

    #[test]
    fn key_order_in_btreemap_does_not_affect_hash() {
        // BTreeMap iterates sorted by key regardless of insertion
        // order, so the canonical encoding is order-independent
        // through the container alone.
        let mut a = BTreeMap::new();
        a.insert("z".to_string(), "1".to_string());
        a.insert("a".to_string(), "2".to_string());
        let mut b = BTreeMap::new();
        b.insert("a".to_string(), "2".to_string());
        b.insert("z".to_string(), "1".to_string());
        assert_eq!(labels_hash(&a), labels_hash(&b));
    }

    #[test]
    fn length_prefix_disambiguates_concatenation() {
        // Without length prefixes, {"ab": "c"} and {"a": "bc"} would
        // both encode as the byte sequence "abc" once delimiters are
        // dropped. The plan's injectivity guarantee depends on this.
        let one = map(&[("ab", "c")]);
        let two = map(&[("a", "bc")]);
        assert_ne!(labels_hash_bytes(&one), labels_hash_bytes(&two));
        assert_ne!(labels_hash(&one), labels_hash(&two));
    }

    #[test]
    fn utf8_byte_length_not_char_count() {
        // The encoding pins UTF-8 byte length, not char count. A
        // multibyte value MUST contribute its byte count to the
        // length prefix. Verify by hand: "é" is two UTF-8 bytes,
        // "ee" is two bytes; both produce 2 as their length prefix
        // and hash to DIFFERENT values because the value bytes
        // differ.
        let a = map(&[("k", "é")]);
        let b = map(&[("k", "ee")]);
        let a_bytes = labels_hash_bytes(&a);
        let b_bytes = labels_hash_bytes(&b);
        // Same encoded length (4 + 4 + 1 + 4 + 2 = 15 bytes each):
        assert_eq!(a_bytes.len(), b_bytes.len());
        // Different content → different hash:
        assert_ne!(labels_hash(&a), labels_hash(&b));
    }
}
