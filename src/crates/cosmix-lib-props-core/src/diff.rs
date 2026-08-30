//! SPEC 07 §3 — leaf-level diff between two property snapshots.
//!
//! The output is the set of leaf paths whose value changed. Returned in
//! deterministic (BTreeMap) order so callers can emit `props.changed`
//! events without reordering across runs. Paths that exist in only one
//! side are reported with the missing side as [`PropValue::Null`].

use std::collections::BTreeSet;

use crate::path::PropPath;
use crate::value::PropValue;

/// Diff two snapshots. Emits one entry per leaf path that differs;
/// object subtrees recurse, non-object differences emit at the current
/// path. A whole subtree being added/removed yields one entry per leaf
/// inside it (callers that want subtree-level events can fold afterwards).
pub fn diff(old: &PropValue, new: &PropValue) -> Vec<(PropPath, PropValue, PropValue)> {
    let mut out = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    walk(&mut stack, old, new, &mut out);
    out
}

fn walk(
    prefix: &mut Vec<String>,
    old: &PropValue,
    new: &PropValue,
    out: &mut Vec<(PropPath, PropValue, PropValue)>,
) {
    if let (PropValue::Object(o), PropValue::Object(n)) = (old, new) {
        let mut keys: BTreeSet<&String> = BTreeSet::new();
        keys.extend(o.keys());
        keys.extend(n.keys());
        for k in keys {
            prefix.push(k.clone());
            let old_child = o.get(k).cloned().unwrap_or(PropValue::Null);
            let new_child = n.get(k).cloned().unwrap_or(PropValue::Null);
            walk(prefix, &old_child, &new_child, out);
            prefix.pop();
        }
        return;
    }
    if old == new {
        return;
    }
    if prefix.is_empty() {
        // Root-level non-object change has no addressable path; skip.
        return;
    }
    if let Ok(path) = PropPath::new(prefix.join(".")) {
        out.push((path, old.clone(), new.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::build_snapshot;

    fn p(s: &str) -> PropPath {
        PropPath::new(s).unwrap()
    }

    #[test]
    fn no_diff_on_identical() {
        let snap = build_snapshot([
            (p("config.bind"), PropValue::from("192.0.2.5:4200")),
            (p("lifecycle.uptime_s"), PropValue::from(42_u64)),
        ]);
        assert!(diff(&snap, &snap).is_empty());
    }

    #[test]
    fn detects_leaf_change() {
        let a = build_snapshot([(p("lifecycle.uptime_s"), PropValue::from(1_u64))]);
        let b = build_snapshot([(p("lifecycle.uptime_s"), PropValue::from(2_u64))]);
        let d = diff(&a, &b);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].0.as_str(), "lifecycle.uptime_s");
        assert_eq!(d[0].1, PropValue::from(1_u64));
        assert_eq!(d[0].2, PropValue::from(2_u64));
    }

    #[test]
    fn detects_list_change() {
        let a = build_snapshot([(
            p("services.registered"),
            PropValue::List(vec![PropValue::from("noded")]),
        )]);
        let b = build_snapshot([(
            p("services.registered"),
            PropValue::List(vec![PropValue::from("noded"), PropValue::from("indexd")]),
        )]);
        let d = diff(&a, &b);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].0.as_str(), "services.registered");
    }

    #[test]
    fn missing_to_present_emits_with_null_old() {
        let a = build_snapshot([(p("config.bind"), PropValue::from("a"))]);
        let b = build_snapshot([
            (p("config.bind"), PropValue::from("a")),
            (p("config.node_name"), PropValue::from("alpha")),
        ]);
        let d = diff(&a, &b);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].0.as_str(), "config.node_name");
        assert_eq!(d[0].1, PropValue::Null);
        assert_eq!(d[0].2, PropValue::from("alpha"));
    }

    #[test]
    fn removed_subtree_yields_per_leaf_entries() {
        let a = build_snapshot([
            (p("svc.a.x"), PropValue::from(1_u64)),
            (p("svc.a.y"), PropValue::from(2_u64)),
        ]);
        let b = build_snapshot([(p("svc.a.x"), PropValue::from(1_u64))]);
        let d = diff(&a, &b);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].0.as_str(), "svc.a.y");
        assert_eq!(d[0].2, PropValue::Null);
    }

    #[test]
    fn deterministic_order() {
        let a = build_snapshot([
            (p("a.b.c"), PropValue::from(1_u64)),
            (p("a.b.a"), PropValue::from(1_u64)),
        ]);
        let b = build_snapshot([
            (p("a.b.c"), PropValue::from(2_u64)),
            (p("a.b.a"), PropValue::from(2_u64)),
        ]);
        let d = diff(&a, &b);
        let paths: Vec<&str> = d.iter().map(|(p, _, _)| p.as_str()).collect();
        assert_eq!(paths, vec!["a.b.a", "a.b.c"]);
    }
}
