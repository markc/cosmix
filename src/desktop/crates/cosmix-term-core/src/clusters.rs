//! Snapshot-owned cluster text. IDs are meaningful only within one generation.
use std::{collections::HashMap, sync::Arc};

pub const MISSING_CLUSTER: u32 = u32::MAX;
const MAX_ENTRIES: usize = 4096;
const MAX_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_CLUSTER_BYTES: usize = 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum CellWidth {
    #[default]
    Narrow,
    Wide,
    Spacer,
    LeadingSpacer,
}

/// Immutable text storage retained by Screens, including CPU band snapshots.
/// Appending uses copy-on-write; rebuilding changes identity, forcing damage.
#[derive(Clone, Debug, Default)]
pub struct Clusters {
    pub(crate) identity: Arc<()>,
    text: Arc<Vec<Arc<str>>>,
}

impl Clusters {
    pub fn get(&self, id: u32) -> Option<&str> {
        id.checked_sub(1)
            .and_then(|i| self.text.get(i as usize))
            .map(AsRef::as_ref)
    }
}

#[derive(Default)]
pub(crate) struct ClusterInterner {
    pub(crate) snapshot: Clusters,
    ids: HashMap<Arc<str>, u32>,
    bytes: usize,
    saturated: bool,
}

impl ClusterInterner {
    /// Only between captures: no ID already written into this frame is reused.
    /// Old Screens keep their own table alive, and PaintState keeps identity.
    pub(crate) fn begin_capture(&mut self) {
        if self.saturated {
            *self = Self::default();
        }
    }

    pub(crate) fn intern(&mut self, text: &str) -> u32 {
        if text.len() > MAX_CLUSTER_BYTES {
            return MISSING_CLUSTER;
        }
        if let Some(id) = self.ids.get(text) {
            return *id;
        }
        if self.ids.len() >= MAX_ENTRIES || self.bytes + text.len() > MAX_BYTES {
            self.saturated = true;
            return MISSING_CLUSTER;
        }
        let text: Arc<str> = text.into();
        self.bytes += text.len();
        let table = Arc::make_mut(&mut self.snapshot.text);
        table.push(text.clone());
        let id = table.len() as u32;
        self.ids.insert(text, id);
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_frames_survive_append_and_bounded_rebuild() {
        let mut interner = ClusterInterner::default();
        let id = interner.intern("👩‍💻");
        let old = interner.snapshot.clone();
        assert_eq!(interner.intern("👩‍💻"), id);
        interner.intern("🇦🇺");
        assert!(Arc::ptr_eq(&old.identity, &interner.snapshot.identity));
        for i in 0..MAX_ENTRIES {
            interner.intern(&format!("e{i}"));
        }
        assert_eq!(interner.intern("new"), MISSING_CLUSTER);
        interner.begin_capture();
        assert!(!Arc::ptr_eq(&old.identity, &interner.snapshot.identity));
        assert_eq!(old.get(id), Some("👩‍💻"));
        assert_eq!(interner.intern("❤️"), 1);
        assert_eq!(old.get(id), Some("👩‍💻"));
        assert_eq!(
            interner.intern(&"x".repeat(MAX_CLUSTER_BYTES + 1)),
            MISSING_CLUSTER
        );
        assert!(interner.snapshot.get(MISSING_CLUSTER).is_none());
    }
}
