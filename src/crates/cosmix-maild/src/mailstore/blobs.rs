//! Account-scoped CAS ownership. Global byte presence is not authorisation.

use super::{AccountId, BlobId, SqliteMailStore, account_id_to_setid};
use anyhow::Result;
use cosmix_mds::{BlobHash, ContainerAttrs, Flags, Mds, SqliteSetTx, blob};
use rusqlite::{OptionalExtension, params};

fn sql_error(e: rusqlite::Error) -> cosmix_mds::Error {
    cosmix_mds::Error::Other(format!("legacy alias: {e}"))
}

fn marker(id: BlobId) -> String {
    serde_json::json!([format!("maild:legacy-blob:{id}")]).to_string()
}

/// Check both sides of the durable migration marker. Returns true only when
/// the preserved UUID and its dedicated holding membership are both present.
fn legacy_status(
    tx: &SqliteSetTx<'_>,
    account: AccountId,
    id: BlobId,
    hash: &BlobHash,
) -> cosmix_mds::Result<bool> {
    let alias: Option<(i32, String, Option<i64>, Option<String>)> = tx.tx().query_row(
        "SELECT account_id, blob_hash, expires_at, temp_item_id FROM blob_refs WHERE blob_id = ?1",
        [id.to_string()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    ).optional().map_err(sql_error)?;
    if let Some((owner, hex, expiry, temp)) = &alias
        && (*owner != account || *hex != blob::hex(hash) || expiry.is_some() || temp.is_some())
    {
        return Err(cosmix_mds::Error::Other(
            "conflicting: existing UUID alias differs".into(),
        ));
    }
    let holds: Vec<String> = {
        let mut query = tx
            .tx()
            .prepare(
                "SELECT i.blob_hash FROM item i JOIN membership m ON m.item_id = i.id \
             JOIN container c ON c.id = m.container_id WHERE m.tags = ?1 AND c.name = ?2",
            )
            .map_err(sql_error)?;
        query
            .query_map(
                params![marker(id), cosmix_mds::container::UPLOAD_STAGING_NAME],
                |r| r.get(0),
            )
            .map_err(sql_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql_error)?
    };
    if holds.len() > 1 || holds.iter().any(|h| h != &blob::hex(hash)) {
        return Err(cosmix_mds::Error::Other(
            "conflicting: legacy holding item differs".into(),
        ));
    }
    // A marker without its alias is not a normal crash outcome: both commit
    // together. Refuse manual/inconsistent state instead of adding another hold.
    if alias.is_none() && !holds.is_empty() {
        return Err(cosmix_mds::Error::Other(
            "conflicting: holding item has no alias".into(),
        ));
    }
    Ok(alias.is_some() && holds.len() == 1)
}

/// Validate before `mds::blob::from_hex`, which slices at byte offsets.
pub(crate) fn parse_hash(value: &str) -> Option<BlobHash> {
    if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    blob::from_hex(value)
}

impl SqliteMailStore {
    /// SELECTs only; an unprovisioned account stays unprovisioned in dry-run.
    pub(crate) fn legacy_ref_status(
        &self,
        account: AccountId,
        id: BlobId,
        hash: &BlobHash,
    ) -> Result<bool> {
        match self.mds().with_set_tx(&account_id_to_setid(account), |tx| {
            legacy_status(tx, account, id, hash)
        }) {
            Ok(present) => Ok(present),
            Err(cosmix_mds::Error::SetNotFound(_)) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Preserve an old upload UUID and retain its bytes through an ordinary
    /// item reference. The hidden membership's sole tag is the migration
    /// marker; it is not an expiring upload. Neither expiry nor mail retention
    /// removes this hold. A later explicit legacy retirement owns its release.
    pub(crate) fn import_legacy_ref(
        &self,
        account: AccountId,
        id: BlobId,
        hash: &BlobHash,
        size: u64,
    ) -> Result<()> {
        let set = self.ensure_account_set(account)?;
        self.mds().with_set_tx(&set, |tx| {
            if legacy_status(tx, account, id, hash)? {
                return Ok(());
            }
            if self.mds().blob_size(hash)? != size {
                return Err(cosmix_mds::Error::Other("corrupt: CAS size changed".into()));
            }
            let staging = tx.ensure_container_by_name(None,
                cosmix_mds::container::UPLOAD_STAGING_NAME,
                &ContainerAttrs { special_use: None, subscribed: false,
                    extra: serde_json::json!({}) })?;
            tx.add_staging_item(&staging, hash, size, Flags(0),
                &[format!("maild:legacy-blob:{id}")])?;
            tx.tx().execute(
                "INSERT INTO blob_refs (blob_id, account_id, blob_hash, created_at, expires_at, temp_item_id) \
                 VALUES (?1, ?2, ?3, ?4, NULL, NULL) ON CONFLICT(blob_id) DO NOTHING",
                params![id.to_string(), account, blob::hex(hash), chrono::Utc::now().timestamp()],
            ).map_err(sql_error)?;
            // Same post-reference existence check as create_email: GC must
            // not leave an acknowledged holding row over vanished CAS bytes.
            if !self.mds().blob_exists(hash)? {
                return Err(cosmix_mds::Error::BlobNotFound(blob::hex(hash)));
            }
            Ok(())
        })?;
        Ok(())
    }

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
