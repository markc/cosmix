//! Validated local-lane reference shared by the share catalogue and media.
#![allow(dead_code)] // P5 foundations; routes/Bus consumers follow the checkpoint.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reference {
    pub blob: String,
    pub size: u64,
    pub mime: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub origin: String,
}

impl Reference {
    pub fn validate(&self) -> Result<(), String> {
        blob_hex(&self.blob)?;
        if self.mime.is_empty()
            || self.mime.len() > 128
            || !self.mime.bytes().all(|b| (0x20..=0x7e).contains(&b))
            || !self.mime.contains('/')
            || self.origin.is_empty()
            || self.origin.len() > 255
            || self.origin.chars().any(char::is_control)
            || self
                .name
                .as_ref()
                .is_some_and(|n| n.len() > 1024 || n.chars().any(char::is_control))
        {
            return Err("invalid_arguments: invalid blob reference metadata".into());
        }
        Ok(())
    }

    pub fn from_json(value: &serde_json::Value) -> Result<Self, String> {
        let reference: Self = serde_json::from_value(value.clone())
            .map_err(|_| "invalid_arguments: invalid blob reference".to_string())?;
        reference.validate()?;
        Ok(reference)
    }
}

pub(crate) fn blob_hex(id: &str) -> Result<&str, String> {
    let hex = id
        .strip_prefix("b3:")
        .ok_or("invalid_arguments: invalid blob id")?;
    if hex.len() != 64 || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err("invalid_arguments: invalid blob id".into());
    }
    Ok(hex)
}

/// Primary FQDN, never an alias. Fixed-length owner fits the lane's 128-byte cap.
pub(crate) fn owner(purpose: &str, primary_fqdn: &str) -> String {
    format!(
        "webd:{purpose}:{}",
        &blake3::hash(primary_fqdn.as_bytes()).to_hex()[..16]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_noncanonical_ids_before_any_byte_indexing() {
        for id in [
            "b3:abc".to_string(),
            format!("b3:{}", "é".repeat(32)),
            format!("b3:{}", "A".repeat(64)),
        ] {
            assert!(blob_hex(&id).is_err());
        }
        assert!(blob_hex(&format!("b3:{}", "a".repeat(64))).is_ok());
    }

    #[test]
    fn owner_is_bounded_and_namespaced() {
        let fqdn = "a".repeat(253);
        assert!(owner("media", &fqdn).len() < 128);
        assert_ne!(owner("media", &fqdn), owner("share", &fqdn));
        assert_ne!(owner("share", "a.example"), owner("share", "b.example"));
    }
}
