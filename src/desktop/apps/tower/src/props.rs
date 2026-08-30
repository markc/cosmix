//! Heterogeneous daemon property-surface state.

use std::collections::BTreeMap;

use serde_json::Value;

pub(crate) const FLAT_PROP_SERVICES: [&str; 7] = [
    "noded", "mon", "log", "indexd", "musicd", "filesd", "interact",
];
pub(crate) const NAMESPACED_PROP_SERVICES: [&str; 2] = ["maild", "webd"];

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PropsAvailability {
    Pending,
    Available,
    NamespaceRequired,
    Unavailable(String),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PropsSurface {
    pub availability: PropsAvailability,
    pub paths: Vec<String>,
    pub snapshot: Option<Value>,
    pub descriptions: BTreeMap<String, Value>,
    pub observed_at_ms: Option<u64>,
}

impl PropsSurface {
    pub(crate) fn pending() -> Self {
        Self {
            availability: PropsAvailability::Pending,
            paths: Vec::new(),
            snapshot: None,
            descriptions: BTreeMap::new(),
            observed_at_ms: None,
        }
    }

    pub(crate) fn namespace_required() -> Self {
        Self {
            availability: PropsAvailability::NamespaceRequired,
            paths: Vec::new(),
            snapshot: None,
            descriptions: BTreeMap::new(),
            observed_at_ms: None,
        }
    }

    pub(crate) fn status_label(&self) -> String {
        match &self.availability {
            PropsAvailability::Pending => "pending".into(),
            PropsAvailability::Available => format!("{} paths", self.paths.len()),
            PropsAvailability::NamespaceRequired => "namespace required".into(),
            PropsAvailability::Unavailable(error) => format!("unknown - {error}"),
        }
    }
}

pub(crate) fn is_flat_surface(service: &str) -> bool {
    FLAT_PROP_SERVICES.contains(&service)
}

pub(crate) fn is_namespaced_surface(service: &str) -> bool {
    NAMESPACED_PROP_SERVICES.contains(&service)
}

pub(crate) fn parse_path_list(body: &str) -> Result<Vec<String>, String> {
    let mut paths: Vec<String> =
        serde_json::from_str(body).map_err(|error| format!("invalid props.list: {error}"))?;
    paths.sort();
    paths.dedup();
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespaced_daemons_are_never_guessed() {
        for service in NAMESPACED_PROP_SERVICES {
            assert!(is_namespaced_surface(service));
            assert_eq!(
                PropsSurface::namespace_required().status_label(),
                "namespace required"
            );
        }
    }

    #[test]
    fn path_lists_are_stable_and_deduplicated() {
        assert_eq!(
            parse_path_list(r#"["z.last","a.first","z.last"]"#).unwrap(),
            ["a.first", "z.last"]
        );
    }
}
