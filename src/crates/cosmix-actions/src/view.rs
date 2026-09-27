//! Canonical plain-sidebar actions for applications with Places and Properties.
use crate::ActionId;
/// Show or hide the application's Places sidebar.
pub const TOGGLE_PLACES: ActionId = ActionId::from_static("view.toggle-places");
/// Show or hide the application's Properties sidebar.
pub const TOGGLE_PROPERTIES: ActionId = ActionId::from_static("view.toggle-properties");
