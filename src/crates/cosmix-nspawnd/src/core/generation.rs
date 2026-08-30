use super::model::{Grant, Tombstone};
use super::name::InstanceName;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GenerationDecision {
    pub generation: u64,
    pub minimum_generation: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum GenerationRefusal {
    #[error("grant missing")]
    GrantMissing,
    #[error("grant name {grant} does not match requested instance {requested}")]
    NameMismatch { requested: String, grant: String },
    #[error("grant owner {owner} does not match local node {local}")]
    OwnerMismatch { owner: String, local: String },
    #[error("requested generation {requested} does not equal grant generation {granted}")]
    GrantMismatch { requested: u64, granted: u64 },
    #[error("grant generation {granted} is below tombstone floor {minimum}")]
    Fenced { granted: u64, minimum: u64 },
}

pub fn evaluate_start(
    requested_name: &InstanceName,
    requested_generation: u64,
    local_node: &str,
    grant: Option<&Grant>,
    tombstone: Option<&Tombstone>,
) -> Result<GenerationDecision, GenerationRefusal> {
    let grant = grant.ok_or(GenerationRefusal::GrantMissing)?;
    if &grant.name != requested_name {
        return Err(GenerationRefusal::NameMismatch {
            requested: requested_name.to_string(),
            grant: grant.name.to_string(),
        });
    }
    if grant.owner != local_node {
        return Err(GenerationRefusal::OwnerMismatch {
            owner: grant.owner.clone(),
            local: local_node.to_owned(),
        });
    }
    if requested_generation != grant.generation {
        return Err(GenerationRefusal::GrantMismatch {
            requested: requested_generation,
            granted: grant.generation,
        });
    }
    let minimum_generation = tombstone.map_or(1, |t| t.minimum_generation);
    if grant.generation < minimum_generation {
        return Err(GenerationRefusal::Fenced {
            granted: grant.generation,
            minimum: minimum_generation,
        });
    }
    Ok(GenerationDecision {
        generation: grant.generation,
        minimum_generation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::{GRANT_SCHEMA, GrantSource, TOMBSTONE_SCHEMA, TombstoneSource};

    fn name(value: &str) -> InstanceName {
        InstanceName::parse(value).unwrap()
    }

    fn grant(owner: &str, generation: u64) -> Grant {
        Grant {
            schema: GRANT_SCHEMA.into(),
            name: name("labspoke"),
            owner: owner.into(),
            generation,
            source: GrantSource {
                kind: "test".into(),
                record_version: 7,
                record_state: "complete".into(),
                record_updated: "now".into(),
            },
            installed_at: "now".into(),
        }
    }

    fn tombstone(floor: u64) -> Tombstone {
        Tombstone {
            schema: TOMBSTONE_SCHEMA.into(),
            name: name("labspoke"),
            minimum_generation: floor,
            moved_to: "beta".into(),
            op_id: "op".into(),
            recorded_at: "now".into(),
            enforced: true,
            source: TombstoneSource {
                kind: "test".into(),
                advisory_source: false,
            },
        }
    }

    #[test]
    fn full_generation_decision_table() {
        let instance = name("labspoke");
        assert_eq!(
            evaluate_start(&instance, 3, "alpha", None, None),
            Err(GenerationRefusal::GrantMissing)
        );
        assert!(matches!(
            evaluate_start(&instance, 3, "alpha", Some(&grant("beta", 3)), None),
            Err(GenerationRefusal::OwnerMismatch { .. })
        ));
        assert!(matches!(
            evaluate_start(&instance, 2, "alpha", Some(&grant("alpha", 3)), None),
            Err(GenerationRefusal::GrantMismatch { .. })
        ));
        assert!(matches!(
            evaluate_start(
                &instance,
                1,
                "alpha",
                Some(&grant("alpha", 1)),
                Some(&tombstone(2))
            ),
            Err(GenerationRefusal::Fenced { .. })
        ));
        assert!(
            evaluate_start(
                &instance,
                2,
                "alpha",
                Some(&grant("alpha", 2)),
                Some(&tombstone(2))
            )
            .is_ok()
        );
        let live = evaluate_start(
            &instance,
            3,
            "alpha",
            Some(&grant("alpha", 3)),
            Some(&tombstone(2)),
        )
        .unwrap();
        assert_eq!(live.generation, 3);
        assert_eq!(live.minimum_generation, 2);
        assert!(matches!(
            evaluate_start(
                &instance,
                4,
                "alpha",
                Some(&grant("alpha", 3)),
                Some(&tombstone(2))
            ),
            Err(GenerationRefusal::GrantMismatch { .. })
        ));
    }

    #[test]
    fn grant_name_must_match() {
        let mut wrong = grant("alpha", 3);
        wrong.name = name("labhub");
        assert!(matches!(
            evaluate_start(&name("labspoke"), 3, "alpha", Some(&wrong), None),
            Err(GenerationRefusal::NameMismatch { .. })
        ));
    }
}
