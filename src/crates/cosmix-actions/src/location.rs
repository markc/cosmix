//! Canonical location editor actions for apps with an editable path bar.

use crate::ActionId;

/// Focus and select the active pane's location text (local UI only).
pub const FOCUS: ActionId = ActionId::from_static("location.focus");
