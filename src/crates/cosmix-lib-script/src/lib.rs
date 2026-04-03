//! cosmix-lib-script — ARexx-style inter-app scripting for cosmix.
//!
//! Provides script discovery, Mix scripting runtime, and dynamic
//! "User" menu generation.
//!
//! # Script format
//!
//! **Mix** — full scripting language with AMP integration:
//!
//! ```mix
//! -- @script Toggle Editor Wrap
//! -- @shortcut Ctrl+Shift+W
//! if port_exists("edit") then
//!     send "edit" ui.get id="edit.word-wrap"
//! end
//! ```
//!
//! # Usage in apps
//!
//! ```ignore
//! // Add User menu (requires "menu" feature)
//! let user_menu = cosmix_script::user_menu("edit");
//!
//! // Handle script actions
//! cosmix_script::handle_script_action(&id, "edit", hub, &vars).await;
//! ```

pub mod types;
pub mod discovery;
pub mod executor;
pub mod mix_runtime;

#[cfg(feature = "dialog")]
pub mod dialog_ext;

#[cfg(feature = "menu")]
pub mod menu;

// Re-exports for convenience
pub use types::{Script, ScriptMeta, ScriptResult};
pub use discovery::{scripts_dir, discover_scripts};
pub use executor::execute_script;
pub use mix_runtime::execute_mix;

#[cfg(feature = "menu")]
pub use menu::{user_menu, handle_script_action};
