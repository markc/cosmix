//! The `PropTree` trait that L1+ daemons implement.

use crate::describe::PropDescribe;
use crate::path::PropPath;
use crate::redact::redact;
use crate::value::PropValue;
use std::collections::BTreeMap;

/// A daemon's property surface.
///
/// Implementers provide three things: a snapshot of the whole tree, the
/// list of all defined leaf paths, and per-path schema descriptions.
/// SPEC 07 §2 maps these directly to `props.get`, `props.list`,
/// `props.describe` commands.
pub trait PropTree {
    /// Full snapshot of the property tree (root). SPEC 07 §2.3.
    fn snapshot(&self) -> PropValue;

    /// Enumerate every leaf path. SPEC 07 §2.2.
    fn list(&self) -> Vec<PropPath>;

    /// Schema for a single path, leaf or subtree. SPEC 07 §2.4.
    /// Returns `None` if the path is unknown.
    fn describe(&self, path: &PropPath) -> Option<PropDescribe>;

    /// Slice the snapshot at `path` (default impl: walk `snapshot()`).
    /// Daemons MAY override for efficiency. Returns `None` if the path
    /// does not resolve.
    fn get(&self, path: &PropPath) -> Option<PropValue> {
        let mut cur = self.snapshot();
        for seg in path.segments() {
            cur = match cur {
                PropValue::Object(mut m) => m.remove(seg)?,
                _ => return None,
            };
        }
        Some(cur)
    }

    /// Returns the snapshot with sensitive paths redacted. Default impl
    /// walks the schema and redacts every leaf whose `describe` returns
    /// `sensitive: true`. Daemons MAY override.
    fn redacted_snapshot(&self) -> PropValue {
        let mut snap = self.snapshot();
        for path in self.list() {
            if let Some(d) = self.describe(&path)
                && d.sensitive
            {
                redact_at_path(&mut snap, &path);
            }
        }
        snap
    }
}

fn redact_at_path(root: &mut PropValue, path: &PropPath) {
    let segs: Vec<&str> = path.segments().collect();
    redact_at_segs(root, &segs);
}

fn redact_at_segs(node: &mut PropValue, segs: &[&str]) {
    if let (PropValue::Object(m), Some((head, rest))) = (node, segs.split_first()) {
        if rest.is_empty() {
            if let Some(v) = m.get_mut(*head) {
                *v = redact(v);
            }
        } else if let Some(child) = m.get_mut(*head) {
            redact_at_segs(child, rest);
        }
    }
}

/// Helper: build a snapshot from a flat `(path, value)` list. Order
/// doesn't matter; conflicting paths (one is a strict prefix of another)
/// resolve in favour of the longer (leaf wins).
pub fn build_snapshot(leaves: impl IntoIterator<Item = (PropPath, PropValue)>) -> PropValue {
    let mut root: BTreeMap<String, PropValue> = BTreeMap::new();
    for (path, value) in leaves {
        let segs: Vec<&str> = path.segments().collect();
        insert_at(&mut root, &segs, value);
    }
    PropValue::Object(root)
}

fn insert_at(node: &mut BTreeMap<String, PropValue>, segs: &[&str], value: PropValue) {
    let (head, rest) = segs.split_first().expect("path has at least one segment");
    if rest.is_empty() {
        node.insert((*head).into(), value);
        return;
    }
    let entry = node
        .entry((*head).into())
        .or_insert_with(|| PropValue::Object(BTreeMap::new()));
    if let PropValue::Object(child) = entry {
        insert_at(child, rest, value);
    } else {
        // Existing leaf at an interior path; replace with object holding child.
        let mut child = BTreeMap::new();
        insert_at(&mut child, rest, value);
        *entry = PropValue::Object(child);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::describe::PropType;

    struct Demo;

    impl PropTree for Demo {
        fn snapshot(&self) -> PropValue {
            build_snapshot([
                (
                    PropPath::new("config.bind").unwrap(),
                    PropValue::from("192.0.2.5:4200"),
                ),
                (
                    PropPath::new("config.token").unwrap(),
                    PropValue::from("supersecret"),
                ),
                (
                    PropPath::new("lifecycle.uptime_s").unwrap(),
                    PropValue::from(42_u64),
                ),
            ])
        }

        fn list(&self) -> Vec<PropPath> {
            vec![
                PropPath::new("config.bind").unwrap(),
                PropPath::new("config.token").unwrap(),
                PropPath::new("lifecycle.uptime_s").unwrap(),
            ]
        }

        fn describe(&self, path: &PropPath) -> Option<PropDescribe> {
            match path.as_str() {
                "config.bind" => Some(PropDescribe::leaf(
                    path.clone(),
                    PropType::String,
                    "bind addr",
                )),
                "config.token" => Some(
                    PropDescribe::leaf(path.clone(), PropType::String, "auth token")
                        .with_sensitive(true),
                ),
                "lifecycle.uptime_s" => Some(PropDescribe::leaf(
                    path.clone(),
                    PropType::Number,
                    "uptime seconds",
                )),
                _ => None,
            }
        }
    }

    #[test]
    fn slice_subtree() {
        let d = Demo;
        let cfg = d.get(&PropPath::new("config").unwrap()).unwrap();
        let m = cfg.as_object().unwrap();
        assert!(m.contains_key("bind"));
        assert!(m.contains_key("token"));
        let leaf = d.get(&PropPath::new("config.bind").unwrap()).unwrap();
        assert_eq!(leaf, PropValue::String("192.0.2.5:4200".into()));
    }

    #[test]
    fn unknown_path_returns_none() {
        let d = Demo;
        assert!(d.get(&PropPath::new("nope").unwrap()).is_none());
        assert!(d.get(&PropPath::new("config.missing").unwrap()).is_none());
    }

    #[test]
    fn redacted_snapshot_hides_sensitive() {
        let d = Demo;
        let r = d.redacted_snapshot();
        let cfg = r.as_object().unwrap()["config"].as_object().unwrap();
        assert_eq!(cfg["bind"], PropValue::String("192.0.2.5:4200".into()));
        assert_eq!(cfg["token"], PropValue::String("***".into()));
    }
}
