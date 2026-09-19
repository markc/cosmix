//! Read-only property projection for `dbusd.props.*`: the supervision
//! state of every adapter under `dbusd.adapters.<name>.*`.

use cosmix_props_core::tree::build_snapshot;
use cosmix_props_core::{PropDescribe, PropPath, PropTree, PropType, PropValue};

use crate::state::{AdapterStateKind, AdapterStatus};

/// Owned projection so no registry lock is held while props-core
/// dispatches.
pub struct DbusdProps {
    leaves: Vec<(PropPath, PropValue)>,
}

impl DbusdProps {
    pub fn new(statuses: &[AdapterStatus]) -> Self {
        let mut leaves = Vec::with_capacity(statuses.len() * 3);
        for status in statuses {
            push(
                &mut leaves,
                &format!("adapters.{}.state", status.name),
                status.state.as_str().into(),
            );
            push(
                &mut leaves,
                &format!("adapters.{}.restarts", status.name),
                status.restarts.into(),
            );
            if let Some(error) = &status.last_error {
                push(
                    &mut leaves,
                    &format!("adapters.{}.last_error", status.name),
                    error.as_str().into(),
                );
            }
        }
        Self { leaves }
    }
}

impl PropTree for DbusdProps {
    fn snapshot(&self) -> PropValue {
        build_snapshot(self.leaves.clone())
    }

    fn list(&self) -> Vec<PropPath> {
        self.leaves.iter().map(|(path, _)| path.clone()).collect()
    }

    fn describe(&self, path: &PropPath) -> Option<PropDescribe> {
        if !self.leaves.iter().any(|(candidate, _)| candidate == path) {
            return None;
        }
        let leaf = path.as_str().rsplit('.').next()?;
        let description = match leaf {
            "state" => {
                let mut description = PropDescribe::leaf(
                    path.clone(),
                    PropType::String,
                    "Supervision state: starting, running, backoff, or disabled.",
                );
                description.enum_values = Some(
                    [
                        AdapterStateKind::Starting,
                        AdapterStateKind::Running,
                        AdapterStateKind::Backoff,
                        AdapterStateKind::Disabled,
                    ]
                    .iter()
                    .map(|state| state.as_str().to_string())
                    .collect(),
                );
                description
            }
            "restarts" => PropDescribe::leaf(
                path.clone(),
                PropType::Number,
                "Relaunches of this adapter's run in this daemon process.",
            ),
            "last_error" => PropDescribe::leaf(
                path.clone(),
                PropType::String,
                "Why this adapter last failed; cleared when it runs healthy again.",
            ),
            _ => return None,
        };
        Some(description)
    }
}

/// `state` is the leaf every consumer watches; `restarts` and
/// `last_error` ride the same diffs. None are transient: each change is
/// a real, non-volatile supervision fact.
fn push(leaves: &mut Vec<(PropPath, PropValue)>, path: &str, value: PropValue) {
    if let Ok(path) = PropPath::new(path) {
        leaves.push((path, value));
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use serde_json::Value;

    use super::*;

    fn status(
        name: &str,
        state: AdapterStateKind,
        restarts: u64,
        error: Option<&str>,
    ) -> AdapterStatus {
        AdapterStatus {
            name: name.into(),
            service: name.into(),
            state,
            restarts,
            last_error: error.map(str::to_string),
            since: SystemTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn snapshot_covers_all_adapters_with_stable_paths() {
        let props = DbusdProps::new(&[
            status("notify", AdapterStateKind::Running, 2, None),
            status("tray", AdapterStateKind::Backoff, 5, Some("panicked: boom")),
        ]);
        let snapshot: Value = (&props.snapshot()).into();
        assert_eq!(snapshot["adapters"]["notify"]["state"], "running");
        assert_eq!(snapshot["adapters"]["notify"]["restarts"], 2);
        assert!(snapshot["adapters"]["notify"].get("last_error").is_none());
        assert_eq!(snapshot["adapters"]["tray"]["state"], "backoff");
        assert_eq!(snapshot["adapters"]["tray"]["last_error"], "panicked: boom");
        let list: Vec<String> = props.list().iter().map(|path| path.to_string()).collect();
        assert!(list.contains(&"adapters.notify.state".to_string()));
        assert!(list.contains(&"adapters.tray.last_error".to_string()));
    }

    #[test]
    fn describe_types_the_supervision_leaves() {
        let props = DbusdProps::new(&[status("notify", AdapterStateKind::Running, 0, None)]);
        let state = props
            .describe(&PropPath::new("adapters.notify.state").unwrap())
            .unwrap();
        assert_eq!(state.ty, PropType::String);
        let restarts = props
            .describe(&PropPath::new("adapters.notify.restarts").unwrap())
            .unwrap();
        assert_eq!(restarts.ty, PropType::Number);
        assert!(
            props
                .describe(&PropPath::new("adapters.notify.absent").unwrap())
                .is_none()
        );
    }
}
