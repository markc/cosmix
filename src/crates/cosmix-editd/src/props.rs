//! `edit.props.*` projection (plan §4.5), served by
//! `cosmix_props_core::bus::dispatch_props`.
//!
//! ```text
//! lifecycle.props_level      "L2"
//! lifecycle.epoch            string
//! lifecycle.volatile         true
//! lifecycle.event_seq        number (transient)
//! lifecycle.publisher_loss   number (transient)
//! buffer_count               number
//! buffers.<bid>.path | opened_as | name | language | eol | bom | dirty | saved_rev | disk | holders
//! buffers.<bid>.rev | lines | bytes | origin_last        (transient)
//! ```
//! `transient` = excluded from `props.changed` (SPEC-07). Each actor pushes a
//! small coarse-state struct to the router on change; the router emits
//! `props.changed` only for the touched buffer's changed non-transient leaves.
//! Never snapshot or diff the whole tree per keystroke.

use cosmix_props_core::{PropDescribe, PropPath, PropTree, PropValue};

/// Owned projection so no daemon lock is held while props-core dispatches.
pub struct EditProps {
    pub leaves: Vec<(PropPath, PropValue)>,
}

impl PropTree for EditProps {
    fn snapshot(&self) -> PropValue {
        cosmix_props_core::tree::build_snapshot(self.leaves.clone())
    }

    fn list(&self) -> Vec<PropPath> {
        self.leaves.iter().map(|(path, _)| path.clone()).collect()
    }

    fn describe(&self, path: &PropPath) -> Option<PropDescribe> {
        let _ = path;
        todo!("E0b: leaf descriptions with the transient set above")
    }
}
