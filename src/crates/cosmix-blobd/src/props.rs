//! Read-only SPEC-07/SPEC-12 property projection for `blob.props.*`.

use cosmix_props_core::tree::build_snapshot;
use cosmix_props_core::{PropDescribe, PropPath, PropTree, PropType, PropValue};

/// Owned projection inputs so no store lock is held while props-core
/// dispatches.
#[derive(Debug, Clone)]
pub struct PropsInput {
    /// `<ip>:<port>` of the byte lane (served in a later slice; the
    /// port is published so a remote can resolve it via
    /// `blobd.props.get` before then).
    pub lane_bind: String,
    pub lane_port: u16,
    pub root: String,
    pub instance: String,
    pub counts_blobs: u64,
    pub counts_pins: u64,
    pub quota_total_used: u64,
    pub quota_total_limit: u64,
    pub generation: u64,
}

/// `blob.props.*` tree: config surface, live counts, quota totals and
/// the lifecycle watermark.
pub struct BlobProps {
    leaves: Vec<(PropPath, PropValue)>,
}

impl BlobProps {
    pub fn new(input: &PropsInput) -> Self {
        let mut leaves = Vec::with_capacity(10);
        push(&mut leaves, "lane.bind", input.lane_bind.clone().into());
        push(&mut leaves, "lane.port", u64::from(input.lane_port).into());
        push(&mut leaves, "root", input.root.clone().into());
        push(&mut leaves, "instance", input.instance.clone().into());
        push(&mut leaves, "counts.blobs", (input.counts_blobs).into());
        push(&mut leaves, "counts.pins", (input.counts_pins).into());
        push(
            &mut leaves,
            "quota.total.used",
            (input.quota_total_used).into(),
        );
        push(
            &mut leaves,
            "quota.total.limit",
            (input.quota_total_limit).into(),
        );
        push(
            &mut leaves,
            "lifecycle.generation",
            (input.generation).into(),
        );
        Self { leaves }
    }
}

impl PropTree for BlobProps {
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
        let mut description = match leaf {
            "bind" => PropDescribe::leaf(
                path.clone(),
                PropType::String,
                "Byte-lane bind address (<ip>:<port>); the listener arrives in a later slice.",
            ),
            "port" => PropDescribe::leaf(
                path.clone(),
                PropType::Number,
                "Byte-lane TCP port. Port discovery is props-only, never the signed inventory.",
            ),
            "root" => PropDescribe::leaf(
                path.clone(),
                PropType::String,
                "Absolute mds root this instance owns (one GC owner per root).",
            ),
            "instance" => PropDescribe::leaf(
                path.clone(),
                PropType::String,
                "Instance name; the Bus service is blobd or blobd-<name>.",
            ),
            "blobs" => PropDescribe::leaf(
                path.clone(),
                PropType::Number,
                "Blobs known to this instance (attrs or pins), not raw CAS files.",
            ),
            "pins" => PropDescribe::leaf(
                path.clone(),
                PropType::Number,
                "Owner pin rows across all blobs.",
            ),
            "used" => PropDescribe::leaf(
                path.clone(),
                PropType::Number,
                "Total accounted bytes (sum of per-owner pinned usage).",
            )
            .with_unit("bytes"),
            "limit" => PropDescribe::leaf(
                path.clone(),
                PropType::Number,
                "Total store cap (quota_total_bytes).",
            )
            .with_unit("bytes"),
            "generation" => PropDescribe::leaf(
                path.clone(),
                PropType::Number,
                "Monotonic mutation counter for this daemon process.",
            ),
            _ => return None,
        };
        // Transient means excluded from props.changed: only the
        // self-referential watermark; every state leaf participates.
        description.transient = matches!(leaf, "generation");
        Some(description)
    }
}

fn push(leaves: &mut Vec<(PropPath, PropValue)>, path: &str, value: PropValue) {
    if let Ok(path) = PropPath::new(path) {
        leaves.push((path, value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn input() -> PropsInput {
        PropsInput {
            lane_bind: "10.42.0.5:4210".into(),
            lane_port: 4210,
            root: "/var/lib/cosmix/blobd".into(),
            instance: "default".into(),
            counts_blobs: 3,
            counts_pins: 5,
            quota_total_used: 1024,
            quota_total_limit: 2048,
            generation: 9,
        }
    }

    #[test]
    fn snapshot_uses_the_documented_paths() {
        let props = BlobProps::new(&input());
        let snapshot: Value = (&props.snapshot()).into();
        assert_eq!(snapshot["lane"]["port"], 4210);
        assert_eq!(snapshot["lane"]["bind"], "10.42.0.5:4210");
        assert_eq!(snapshot["root"], "/var/lib/cosmix/blobd");
        assert_eq!(snapshot["counts"]["blobs"], 3);
        assert_eq!(snapshot["counts"]["pins"], 5);
        assert_eq!(snapshot["quota"]["total"]["used"], 1024);
        assert_eq!(snapshot["quota"]["total"]["limit"], 2048);
        assert_eq!(snapshot["lifecycle"]["generation"], 9);
    }

    #[test]
    fn generation_is_transient_and_state_leaves_are_not() {
        let props = BlobProps::new(&input());
        let generation = PropPath::new("lifecycle.generation").unwrap();
        let blobs = PropPath::new("counts.blobs").unwrap();
        let port = PropPath::new("lane.port").unwrap();
        assert!(props.describe(&generation).unwrap().transient);
        assert!(!props.describe(&blobs).unwrap().transient);
        assert!(!props.describe(&port).unwrap().transient);
    }

    #[test]
    fn describe_rejects_unknown_paths() {
        let props = BlobProps::new(&input());
        assert!(
            props
                .describe(&PropPath::new("lane.host").unwrap())
                .is_none()
        );
        assert!(props.describe(&PropPath::new("nope").unwrap()).is_none());
    }
}
