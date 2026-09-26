//! The blob reference wire shape (D6).
//!
//! `{"blob":"b3:<64 hex>","size":N,"mime":"…","name":"…"?,
//! "origin":"<node name>"}` — the small JSON value every verb carries
//! and every app understands. `origin` is the node name that holds the
//! bytes (props-legible, never an IP). References are what travel over
//! the Bus; bytes never do.

use cosmix_mds::blob;
use cosmix_mds::types::BlobHash;
use serde_json::Value;

/// The reference algorithm prefix. One algorithm (BLAKE3) or dedup
/// breaks (D3); the prefix leaves room for another one day.
pub const HASH_PREFIX: &str = "b3:";

/// A blob reference: the public wire currency of the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub hash: BlobHash,
    pub size: u64,
    pub mime: String,
    /// Original file name hint, when the ingester knew one.
    pub name: Option<String>,
    /// Node name of the holder — never an IP.
    pub origin: String,
}

impl Reference {
    /// The D6 JSON object for this reference.
    pub fn to_json(&self) -> Value {
        let mut v = serde_json::json!({
            "blob": blob_id(&self.hash),
            "size": self.size,
            "mime": self.mime,
            "origin": self.origin,
        });
        if let Some(name) = &self.name {
            v["name"] = Value::String(name.clone());
        }
        v
    }

    /// Parse a reference JSON object back. `blob` must be a valid
    /// `b3:` id; `size` a non-negative integer; `mime`/`origin`
    /// strings; `name` optional string.
    pub fn from_json(v: &Value) -> Option<Self> {
        let id = v.get("blob")?.as_str()?;
        let hash = parse_blob_id(id)?;
        let size = v.get("size")?.as_u64()?;
        let mime = v.get("mime")?.as_str()?.to_string();
        let origin = v.get("origin")?.as_str()?.to_string();
        let name = v.get("name").and_then(|n| n.as_str()).map(String::from);
        Some(Self {
            hash,
            size,
            mime,
            name,
            origin,
        })
    }
}

/// The `b3:<64 hex>` blob id for `hash` (lowercase, 67 chars).
pub fn blob_id(hash: &BlobHash) -> String {
    format!("{HASH_PREFIX}{}", blob::hex(hash))
}

/// Parse a `b3:<64 hex>` id into a [`BlobHash`]. `None` for any other
/// prefix, length, or non-hex characters.
pub fn parse_blob_id(s: &str) -> Option<BlobHash> {
    let hex = s.strip_prefix(HASH_PREFIX)?;
    // from_hex accepts uppercase; ids are emitted lowercase, so
    // normalise the accepted input to it as well — a mixed-case id is
    // the same hash either way.
    blob::from_hex(&hex.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Reference {
        Reference {
            hash: cosmix_mds::blob::hash_bytes(b"reference fixture"),
            size: 17,
            mime: "image/png".into(),
            name: Some("shot.png".into()),
            origin: "alpha".into(),
        }
    }

    #[test]
    fn blob_id_is_b3_plus_64_lowercase_hex() {
        let r = fixture();
        let id = blob_id(&r.hash);
        assert!(id.starts_with("b3:"));
        assert_eq!(id.len(), 3 + 64);
        assert!(
            id[3..]
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }

    #[test]
    fn json_round_trip_preserves_every_field() {
        let r = fixture();
        let json = r.to_json();
        assert_eq!(Reference::from_json(&json).unwrap(), r);
        assert_eq!(json["name"], "shot.png");
        assert_eq!(json["origin"], "alpha");
    }

    #[test]
    fn json_omits_name_when_absent() {
        let mut r = fixture();
        r.name = None;
        let json = r.to_json();
        assert!(json.get("name").is_none());
        assert_eq!(Reference::from_json(&json).unwrap(), r);
    }

    #[test]
    fn parse_accepts_only_the_b3_namespace() {
        let id = blob_id(&fixture().hash);
        assert!(parse_blob_id(&id).is_some());
        // Wrong prefix, wrong length, non-hex, empty.
        assert!(parse_blob_id(&format!("sha256:{}", &id[3..])).is_none());
        assert!(parse_blob_id("b3:abcd").is_none());
        assert!(
            parse_blob_id("b3:zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz")
                .is_none()
        );
        assert!(parse_blob_id("").is_none());
        assert!(parse_blob_id("b3:").is_none());
        // Uppercase hex is the same hash.
        assert_eq!(
            parse_blob_id(&format!("b3:{}", id[3..].to_ascii_uppercase())),
            parse_blob_id(&id)
        );
    }

    #[test]
    fn from_json_rejects_missing_or_mistyped_fields() {
        let r = fixture();
        let json = r.to_json();
        for mangle in [
            |v: &mut Value| {
                v["blob"] = Value::from("b3:tooshort");
            },
            |v: &mut Value| {
                v["size"] = Value::from(-1);
            },
            |v: &mut Value| {
                v["mime"] = Value::from(7);
            },
            |v: &mut Value| {
                v["origin"] = Value::Null;
            },
        ] {
            let mut broken = json.clone();
            mangle(&mut broken);
            assert!(Reference::from_json(&broken).is_none());
        }
        assert!(Reference::from_json(&serde_json::json!({})).is_none());
    }
}
