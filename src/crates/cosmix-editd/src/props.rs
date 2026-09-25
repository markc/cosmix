//! `edit.props.*` projection (plan §4.5), served by
//! `cosmix_props_core::bus::dispatch_props`.
//!
//! ```text
//! lifecycle.props_level      "L2"
//! lifecycle.epoch            string
//! lifecycle.volatile         !(recovery enabled && ok) (ced E1 plan §5.1)
//! lifecycle.recovery_ok      bool
//! lifecycle.recovery_unsynced number (transient)
//! lifecycle.event_seq        number (transient)
//! lifecycle.publisher_loss   number (transient)
//! buffer_count               number
//! buffers.<bid>.path | opened_as | name | language | eol | bom | dirty | saved_rev | disk | holders
//! buffers.<bid>.recovery_id | recovered
//! buffers.<bid>.rev | lines | bytes | origin_last        (transient)
//! ```
//! `transient` = excluded from `props.changed` (SPEC-07). Each actor pushes a
//! small coarse-state struct to the router on change; the router emits
//! `props.changed` only for the touched buffer's changed non-transient leaves.
//! Never snapshot or diff the whole tree per keystroke.

use std::collections::BTreeMap;

use cosmix_edit_core::buffer::Eol;
use cosmix_edit_core::wire::{BufferId, DiskState};
use cosmix_props_core::{PropDescribe, PropPath, PropTree, PropType, PropValue};

/// One buffer's coarse state, pushed by its actor on every change.
#[derive(Debug, Clone, PartialEq)]
pub struct BufferProps {
    pub path: Option<String>,
    pub opened_as: Option<String>,
    pub name: Option<String>,
    pub language: String,
    pub eol: Eol,
    pub bom: bool,
    pub dirty: bool,
    pub saved_rev: Option<u64>,
    pub disk: DiskState,
    pub rev: u64,
    pub lines: usize,
    pub bytes: usize,
    pub origin_last: Option<String>,
    /// Stable id of this buffer's recovery files (ced E1 plan §5.1).
    pub recovery_id: String,
    /// Restored from recovery files at this daemon start.
    pub recovered: bool,
}

/// The recovery half of `lifecycle.*` (disabled: volatile, not ok).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryLifecycle {
    pub volatile: bool,
    pub ok: bool,
    /// Records written but not yet synced (transient).
    pub unsynced: u64,
}

impl Default for RecoveryLifecycle {
    fn default() -> Self {
        Self { volatile: true, ok: false, unsynced: 0 }
    }
}

/// Owned projection so no daemon lock is held while props-core dispatches.
pub struct EditProps {
    pub leaves: Vec<(PropPath, PropValue)>,
}

const BUFFER_LEAVES: &[(&str, PropType, bool, &str)] = &[
    ("path", PropType::String, false, "Canonical file the buffer is bound to (null for scratch)."),
    ("opened_as", PropType::String, false, "The path spelling given to edit.open."),
    ("name", PropType::String, false, "File name (null for scratch)."),
    ("language", PropType::String, false, "Detected or overridden language id."),
    ("eol", PropType::String, false, "Line endings found at load: lf, crlf, mixed or none."),
    ("bom", PropType::Bool, false, "Whether the file had a UTF-8 BOM (restored on save)."),
    ("dirty", PropType::Bool, false, "Unsaved changes exist (kept in recovery files unless lifecycle.volatile)."),
    ("saved_rev", PropType::Number, false, "The rev last saved or loaded (null if never)."),
    ("disk", PropType::String, false, "clean | modified | deleted | none | unwatched."),
    ("holders", PropType::List, false, "Caller keys holding the buffer open."),
    ("recovery_id", PropType::String, false, "Stable id of the buffer's recovery files (kept across restarts)."),
    ("recovered", PropType::Bool, false, "Restored from recovery files at this daemon start."),
    ("rev", PropType::Number, true, "Current revision (per-edit; transient)."),
    ("lines", PropType::Number, true, "Line count (transient)."),
    ("bytes", PropType::Number, true, "Text bytes, BOM excluded (transient)."),
    ("origin_last", PropType::String, true, "Origin of the latest edit (transient)."),
];

fn opt_str(v: &Option<String>) -> PropValue {
    v.as_ref().map(|s| PropValue::String(s.clone())).unwrap_or(PropValue::Null)
}

fn eol_str(eol: Eol) -> &'static str {
    match eol {
        Eol::Lf => "lf",
        Eol::Crlf => "crlf",
        Eol::Mixed => "mixed",
        Eol::None => "none",
    }
}

pub fn disk_str(disk: DiskState) -> &'static str {
    match disk {
        DiskState::Clean => "clean",
        DiskState::Modified => "modified",
        DiskState::Deleted => "deleted",
        DiskState::None => "none",
        DiskState::Unwatched => "unwatched",
    }
}

/// Every leaf of one buffer, with whether it is transient.
pub fn buffer_leaves(bid: &str, p: &BufferProps, holders: &[String]) -> Vec<(PropPath, PropValue, bool)> {
    let value = |leaf: &str| -> PropValue {
        match leaf {
            "path" => opt_str(&p.path),
            "opened_as" => opt_str(&p.opened_as),
            "name" => opt_str(&p.name),
            "language" => p.language.clone().into(),
            "eol" => eol_str(p.eol).into(),
            "bom" => p.bom.into(),
            "dirty" => p.dirty.into(),
            "saved_rev" => p.saved_rev.map(PropValue::UInt).unwrap_or(PropValue::Null),
            "disk" => disk_str(p.disk).into(),
            "holders" => PropValue::List(holders.iter().map(|h| PropValue::String(h.clone())).collect()),
            "recovery_id" => p.recovery_id.clone().into(),
            "recovered" => p.recovered.into(),
            "rev" => PropValue::UInt(p.rev),
            "lines" => PropValue::UInt(p.lines as u64),
            "bytes" => PropValue::UInt(p.bytes as u64),
            "origin_last" => opt_str(&p.origin_last),
            _ => PropValue::Null,
        }
    };
    BUFFER_LEAVES
        .iter()
        .filter_map(|(leaf, _, transient, _)| {
            PropPath::new(format!("buffers.{bid}.{leaf}")).ok().map(|path| (path, value(leaf), *transient))
        })
        .collect()
}

impl EditProps {
    /// The whole tree (only on `props.get|list|describe`, never per edit).
    pub fn build(
        epoch: &str,
        event_seq: u64,
        publisher_loss: u64,
        recovery: RecoveryLifecycle,
        buffers: &BTreeMap<BufferId, (BufferProps, Vec<String>)>,
    ) -> Self {
        let mut leaves = Vec::new();
        let mut push = |path: &str, value: PropValue| {
            if let Ok(path) = PropPath::new(path) {
                leaves.push((path, value));
            }
        };
        push("lifecycle.props_level", "L2".into());
        push("lifecycle.epoch", epoch.into());
        push("lifecycle.volatile", recovery.volatile.into());
        push("lifecycle.recovery_ok", recovery.ok.into());
        push("lifecycle.recovery_unsynced", PropValue::UInt(recovery.unsynced));
        push("lifecycle.event_seq", PropValue::UInt(event_seq));
        push("lifecycle.publisher_loss", PropValue::UInt(publisher_loss));
        push("buffer_count", PropValue::UInt(buffers.len() as u64));
        for (bid, (props, holders)) in buffers {
            for (path, value, _) in buffer_leaves(bid, props, holders) {
                leaves.push((path, value));
            }
        }
        Self { leaves }
    }
}

impl PropTree for EditProps {
    fn snapshot(&self) -> PropValue {
        cosmix_props_core::tree::build_snapshot(self.leaves.clone())
    }

    fn list(&self) -> Vec<PropPath> {
        self.leaves.iter().map(|(path, _)| path.clone()).collect()
    }

    fn describe(&self, path: &PropPath) -> Option<PropDescribe> {
        let segs: Vec<&str> = path.segments().collect();
        let children = |prefix: &PropPath| -> Vec<PropPath> {
            let depth = prefix.segments().count() + 1;
            let mut out: Vec<PropPath> = self
                .leaves
                .iter()
                .filter(|(p, _)| p.starts_with(prefix))
                .filter_map(|(p, _)| PropPath::new(p.segments().take(depth).collect::<Vec<_>>().join(".")).ok())
                .collect();
            out.dedup();
            out
        };
        let object = |description: &str| {
            let mut d = PropDescribe::leaf(path.clone(), PropType::Object, description);
            d.children = Some(children(path));
            Some(d)
        };
        match segs.as_slice() {
            ["lifecycle"] => object("Daemon lifecycle."),
            ["lifecycle", "props_level"] => {
                Some(PropDescribe::leaf(path.clone(), PropType::String, "SPEC-07 props level (L2)."))
            }
            ["lifecycle", "epoch"] => Some(PropDescribe::leaf(
                path.clone(),
                PropType::String,
                "Fresh 8-hex id per daemon start; part of every buffer id.",
            )),
            ["lifecycle", "volatile"] => Some(PropDescribe::leaf(
                path.clone(),
                PropType::Bool,
                "Unsaved text is lost on a daemon stop: recovery is disabled or degraded.",
            )),
            ["lifecycle", "recovery_ok"] => Some(PropDescribe::leaf(
                path.clone(),
                PropType::Bool,
                "Recovery files are enabled and every dirty buffer is protected.",
            )),
            ["lifecycle", "recovery_unsynced"] => Some(
                PropDescribe::leaf(path.clone(), PropType::Number, "Recovery records written but not yet synced.")
                    .with_transient(true),
            ),
            ["lifecycle", "event_seq"] => Some(
                PropDescribe::leaf(path.clone(), PropType::Number, "Last event_seq published on edit.changed.")
                    .with_transient(true),
            ),
            ["lifecycle", "publisher_loss"] => Some(
                PropDescribe::leaf(path.clone(), PropType::Number, "Events dropped (each announced by a resync).")
                    .with_transient(true),
            ),
            ["buffer_count"] => Some(PropDescribe::leaf(path.clone(), PropType::Number, "Open buffers.")),
            ["buffers"] => object("Open buffers by id."),
            ["buffers", bid] if self.leaves.iter().any(|(p, _)| p.as_str().starts_with(&format!("buffers.{bid}."))) => {
                object("One buffer's coarse state.")
            }
            ["buffers", bid, leaf] => {
                if !self.leaves.iter().any(|(p, _)| p == path) {
                    let _ = bid;
                    return None;
                }
                let (_, ty, transient, description) = BUFFER_LEAVES.iter().find(|(name, ..)| name == leaf)?;
                Some(PropDescribe::leaf(path.clone(), *ty, *description).with_transient(*transient))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props() -> BufferProps {
        BufferProps {
            path: Some("/tmp/x.mix".into()),
            opened_as: Some("/tmp/x.mix".into()),
            name: Some("x.mix".into()),
            language: "mix".into(),
            eol: Eol::Lf,
            bom: false,
            dirty: true,
            saved_rev: Some(0),
            disk: DiskState::Clean,
            rev: 3,
            lines: 4,
            bytes: 20,
            origin_last: Some("agent:a".into()),
            recovery_id: "5f0c2a9e1b7d4c33".into(),
            recovered: false,
        }
    }

    #[test]
    fn tree_lists_describes_and_marks_transient() {
        let mut buffers = BTreeMap::new();
        buffers.insert("b1_9f2c41a7".to_string(), (props(), vec!["anon".to_string()]));
        let rec = RecoveryLifecycle { volatile: false, ok: true, unsynced: 2 };
        let tree = EditProps::build("9f2c41a7", 5, 0, rec, &buffers);
        let list = tree.list();
        assert!(list.iter().any(|p| p.as_str() == "buffers.b1_9f2c41a7.dirty"));
        let rev = PropPath::new("buffers.b1_9f2c41a7.rev").unwrap();
        assert!(tree.describe(&rev).unwrap().transient);
        let dirty = PropPath::new("buffers.b1_9f2c41a7.dirty").unwrap();
        assert!(!tree.describe(&dirty).unwrap().transient);
        for path in &list {
            assert!(tree.describe(path).is_some(), "{path:?} undescribed");
        }
        let seq = PropPath::new("lifecycle.event_seq").unwrap();
        assert!(tree.describe(&seq).unwrap().transient);
        let response = cosmix_props_core::bus::dispatch_props(&tree, "get", None, true);
        assert_eq!(response.rc, 0);
        let v: serde_json::Value = serde_json::from_str(&response.body).unwrap();
        assert_eq!(v["buffers"]["b1_9f2c41a7"]["language"], "mix");
        assert_eq!(v["lifecycle"]["volatile"], false);
        assert_eq!(v["lifecycle"]["recovery_ok"], true);
        assert_eq!(v["buffers"]["b1_9f2c41a7"]["recovery_id"], "5f0c2a9e1b7d4c33");
        let unsynced = PropPath::new("lifecycle.recovery_unsynced").unwrap();
        assert!(tree.describe(&unsynced).unwrap().transient);
    }
}
