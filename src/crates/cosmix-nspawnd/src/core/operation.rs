use serde::Serialize;

use super::model::{CarriedGrant, OperationVerb};
use super::name::InstanceName;

/// Token-free, deterministic mutation identity. Authentication material is
/// deliberately absent so it can never enter the durable request hash.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MutationFingerprint<'a> {
    pub schema: &'a str,
    pub verb: OperationVerb,
    pub name: &'a InstanceName,
    pub generation: u64,
    pub request_id: &'a str,
    pub grant: Option<&'a CarriedGrant>,
}

pub fn request_hash(value: &MutationFingerprint<'_>) -> Result<String, serde_json::Error> {
    let bytes = serde_json::to_vec(value)?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_hash_is_stable_and_has_no_token_slot() {
        let name = InstanceName::parse("labspoke").unwrap();
        let fingerprint = MutationFingerprint {
            schema: "cosmix.nspawnd.request.v1",
            verb: OperationVerb::Start,
            name: &name,
            generation: 3,
            request_id: "req-1",
            grant: None,
        };
        assert_eq!(
            request_hash(&fingerprint).unwrap(),
            request_hash(&fingerprint).unwrap()
        );
        assert!(
            !serde_json::to_string(&fingerprint)
                .unwrap()
                .contains("token")
        );
    }
}
