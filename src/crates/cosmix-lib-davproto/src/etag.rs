//! Strong content-hash ETags.
//!
//! A resource's ETag is a quoted blake3 digest of its canonical content
//! bytes. The DAV server hashes the canonical-serialized JSCalendar /
//! JSContact `data` blob (not the emitted iCal/vCard, whose formatting
//! could drift across `icalendar` versions), so the ETag is stable across
//! reads and changes iff the stored content changes — the precondition
//! that protects `If-Match` updates from lost writes.

/// Strong ETag over `bytes`: `"<blake3-hex>"` (quoted, per RFC 7232 §2.3).
pub fn strong(bytes: &[u8]) -> String {
    format!("\"{}\"", blake3::hash(bytes).to_hex())
}

/// Hash a canonical JSON value (Object keys are a sorted BTreeMap, so
/// re-serialization is deterministic → stable across reads).
fn for_value(v: &serde_json::Value) -> String {
    strong(&serde_json::to_vec(v).unwrap_or_default())
}

/// ETag for a calendar event, covering **every** field its iCalendar
/// representation is built from — the indexed columns (`title`/`start`/
/// `end`/`updated`) *and* the JSCalendar `data` blob. Hashing the source
/// tuple (not the emitted iCal text) keeps the ETag exact w.r.t. the
/// representation yet stable across `icalendar`-crate version bumps. A
/// change to any emitted field changes the ETag; a deploy that only
/// reformats the iCal does not.
pub fn for_event(
    uid: &str,
    title: Option<&str>,
    start: Option<&str>,
    end: Option<&str>,
    updated: Option<&str>,
    data: &serde_json::Value,
) -> String {
    for_value(&serde_json::json!([uid, title, start, end, updated, data]))
}

/// ETag for a contact, covering every field its vCard representation is
/// built from (UID, FN source, EMAIL, ORG, and the JSContact `data`).
pub fn for_contact(
    uid: &str,
    full_name: Option<&str>,
    email: Option<&str>,
    company: Option<&str>,
    data: &serde_json::Value,
) -> String {
    for_value(&serde_json::json!([uid, full_name, email, company, data]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_and_quoted() {
        let a = strong(b"hello");
        assert!(a.starts_with('"') && a.ends_with('"'));
        assert_eq!(a, strong(b"hello"));
        assert_ne!(a, strong(b"world"));
    }

    #[test]
    fn event_etag_covers_indexed_columns_and_data() {
        let data = serde_json::json!({"description": "x"});
        let base = for_event("u", Some("T"), Some("S"), Some("E"), Some("U"), &data);
        // Same inputs → same ETag.
        assert_eq!(
            base,
            for_event("u", Some("T"), Some("S"), Some("E"), Some("U"), &data)
        );
        // A change to an indexed column (title) changes the ETag even
        // though `data` is unchanged — the M2 representation-drift bug.
        assert_ne!(
            base,
            for_event("u", Some("T2"), Some("S"), Some("E"), Some("U"), &data)
        );
        // A change to `data` changes it too.
        assert_ne!(
            base,
            for_event(
                "u",
                Some("T"),
                Some("S"),
                Some("E"),
                Some("U"),
                &serde_json::json!({"description": "y"})
            )
        );
    }

    #[test]
    fn contact_etag_key_order_stable() {
        let a = serde_json::json!({"a": 1, "b": 2});
        let b = serde_json::json!({"b": 2, "a": 1});
        assert_eq!(
            for_contact("u", Some("N"), None, None, &a),
            for_contact("u", Some("N"), None, None, &b)
        );
    }
}
