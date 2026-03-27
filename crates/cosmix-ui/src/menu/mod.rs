mod types;
mod builder;
mod component;
mod shortcuts;

pub use types::{MenuAction, MenuBarDef, MenuItem, Shortcut};
pub use builder::{
    action, action_shortcut, separator, submenu, menubar,
    standard_file_menu, standard_help_menu,
};
pub use component::MenuBar;

#[cfg(feature = "hub")]
pub use builder::{amp_action, amp_action_args};

#[cfg(feature = "hub")]
pub use shortcuts::use_menu_shortcuts;
#[cfg(not(feature = "hub"))]
pub use shortcuts::use_menu_shortcuts;
