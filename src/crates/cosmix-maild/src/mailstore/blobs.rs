//! Account-scoped CAS ownership. Global byte presence is not authorisation.

use super::{AccountId, SqliteMailStore, account_id_to_setid};
use anyhow::Result;
use cosmix_mds::{BlobHash, Mds, blob};
use rusqlite::params;

/// Validate before `mds::blob::from_hex`, which slices at byte offsets.
pub(crate) fn parse_hash(value: &str) -> Option<BlobHash> {
    if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    blob::from_hex(value)
}

impl SqliteMailStore {
    /// Live mail membership or an unexpired/persistent alias in this account.
    /// No provisioning on a read. Storage failures propagate, never authorise.
    pub fn owns_blob_hash(&self, account: AccountId, hash: &BlobHash) -> Result<bool> {
        let set = account_id_to_setid(account);
        let hex = blob::hex(hash);
        let result = self.mds().with_set_tx(&set, |tx| {
            tx.tx()
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM item i \
                 JOIN mail_envelopes e ON e.item_id = i.id \
                 JOIN membership m ON m.item_id = i.id \
                 JOIN container c ON c.id = m.container_id \
                 WHERE i.blob_hash = ?1 AND c.name != ?4) \
                 OR EXISTS(SELECT 1 FROM blob_refs r \
                 JOIN blobs_db.blob b ON b.hash = r.blob_hash \
                 WHERE r.account_id = ?2 AND r.blob_hash = ?1 \
                 AND (r.expires_at IS NULL OR r.expires_at > ?3) \
                 AND (r.temp_item_id IS NULL OR EXISTS(SELECT 1 FROM item i \
                     JOIN membership m ON m.item_id = i.id \
                     WHERE i.id = r.temp_item_id AND i.blob_hash = r.blob_hash)))",
                    params![
                        hex,
                        account,
                        chrono::Utc::now().timestamp(),
                        cosmix_mds::container::UPLOAD_STAGING_NAME
                    ],
                    |r| r.get::<_, bool>(0),
                )
                .map_err(|e| cosmix_mds::Error::Other(format!("blob ownership: {e}")))
        });
        match result {
            Ok(owned) => Ok(owned && self.mds().blob_exists(hash)?),
            Err(cosmix_mds::Error::SetNotFound(_)) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_hash;

    #[test]
    fn hash_parser_rejects_non_ascii_before_slicing() {
        assert!(parse_hash(&"a".repeat(64)).is_some());
        assert!(parse_hash(&"A".repeat(64)).is_some());
        assert!(parse_hash(&format!("aé{}", "b".repeat(61))).is_none());
        assert!(parse_hash(&"g".repeat(64)).is_none());
        assert!(parse_hash("abc").is_none());
    }
}
