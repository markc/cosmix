//! Canonical FileMgr action ids shared by its menus and packaged keymap.

use crate::{ActionId, theme};

/// Open the selected file or folder.
pub const FILE_OPEN: ActionId = ActionId::from_static("file.open");
/// Create a folder in the active pane.
pub const FILE_NEW_FOLDER: ActionId = ActionId::from_static("file.new-folder");
/// Rename the selected file or folder.
pub const FILE_RENAME: ActionId = ActionId::from_static("file.rename");
/// Copy the selection to the other pane.
pub const FILE_COPY: ActionId = ActionId::from_static("file.copy-other-pane");
/// Move the selection to the other pane.
pub const FILE_MOVE: ActionId = ActionId::from_static("file.move-other-pane");
/// Permanently delete the selection after confirmation.
pub const FILE_DELETE: ActionId = ActionId::from_static("file.delete");
/// Quit FileMgr.
pub const APP_QUIT: ActionId = ActionId::from_static("app.quit");

/// Navigate backwards in active-pane history.
pub const NAV_BACK: ActionId = ActionId::from_static("nav.back");
/// Navigate forwards in active-pane history.
pub const NAV_FORWARD: ActionId = ActionId::from_static("nav.forward");
/// Navigate to the active pane's parent folder.
pub const NAV_PARENT: ActionId = ActionId::from_static("nav.parent");
/// Navigate the active pane to the user's home folder.
pub const NAV_HOME: ActionId = ActionId::from_static("nav.home");
/// Switch active pane.
pub const NAV_SWITCH_PANE: ActionId = ActionId::from_static("nav.switch-pane");
/// Navigate to a typed local place path.
pub const PLACE_OPEN: ActionId = ActionId::from_static("place.open");

/// Refresh the active pane.
pub const VIEW_REFRESH: ActionId = ActionId::from_static("view.refresh");
/// Toggle hidden-file visibility.
pub const VIEW_TOGGLE_HIDDEN: ActionId = ActionId::from_static("view.toggle-hidden");
/// Sort the active pane by name.
pub const VIEW_SORT_NAME: ActionId = ActionId::from_static("view.sort-name");
/// Sort the active pane by size.
pub const VIEW_SORT_SIZE: ActionId = ActionId::from_static("view.sort-size");
/// Sort the active pane by modification time.
pub const VIEW_SORT_MODIFIED: ActionId = ActionId::from_static("view.sort-modified");

/// Select the next row.
pub const SELECT_NEXT: ActionId = ActionId::from_static("selection.next");
/// Select the previous row.
pub const SELECT_PREVIOUS: ActionId = ActionId::from_static("selection.previous");
/// Select the first row.
pub const SELECT_FIRST: ActionId = ActionId::from_static("selection.first");
/// Select the last row.
pub const SELECT_LAST: ActionId = ActionId::from_static("selection.last");

/// Every FileMgr menu action in declaration order.
pub const MENU_ACTION_IDS: [ActionId; 24] = [
    FILE_OPEN,
    FILE_NEW_FOLDER,
    FILE_RENAME,
    FILE_COPY,
    FILE_MOVE,
    FILE_DELETE,
    APP_QUIT,
    NAV_BACK,
    NAV_FORWARD,
    NAV_PARENT,
    NAV_HOME,
    NAV_SWITCH_PANE,
    VIEW_REFRESH,
    VIEW_TOGGLE_HIDDEN,
    VIEW_SORT_NAME,
    VIEW_SORT_SIZE,
    VIEW_SORT_MODIFIED,
    theme::MODE_TOGGLE,
    theme::SCHEME_OCEAN,
    theme::SCHEME_CRIMSON,
    theme::SCHEME_STONE,
    theme::SCHEME_FOREST,
    theme::SCHEME_SUNSET,
    theme::SCHEME_MONO,
];

/// Static actions expected in FileMgr's checked-in default keymap.
pub const DEFAULT_KEYMAP_ACTION_IDS: [ActionId; 28] = [
    FILE_OPEN,
    FILE_NEW_FOLDER,
    FILE_RENAME,
    FILE_COPY,
    FILE_MOVE,
    FILE_DELETE,
    APP_QUIT,
    NAV_BACK,
    NAV_FORWARD,
    NAV_PARENT,
    NAV_HOME,
    NAV_SWITCH_PANE,
    VIEW_REFRESH,
    VIEW_TOGGLE_HIDDEN,
    VIEW_SORT_NAME,
    VIEW_SORT_SIZE,
    VIEW_SORT_MODIFIED,
    SELECT_NEXT,
    SELECT_PREVIOUS,
    SELECT_FIRST,
    SELECT_LAST,
    theme::MODE_TOGGLE,
    theme::SCHEME_OCEAN,
    theme::SCHEME_CRIMSON,
    theme::SCHEME_STONE,
    theme::SCHEME_FOREST,
    theme::SCHEME_SUNSET,
    theme::SCHEME_MONO,
];
