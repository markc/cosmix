//! cosmix-lib-script — ARexx-style inter-app scripting for cosmix.
//!
//! Provides script discovery, TOML-based script definitions, Mix scripting,
//! variable substitution, sequential AMP command execution, and dynamic
//! "User" menu generation.
//!
//! # Script formats
//!
//! **TOML** — step-based AMP command sequences in
//! `~/.config/cosmix/scripts/{service}/`:
//!
//! ```toml
//! [script]
//! name = "Preview in Viewer"
//! shortcut = "Ctrl+Shift+V"
//!
//! [[steps]]
//! to = "view"
//! command = "view.open"
//! args = '{"path": "$CURRENT_FILE"}'
//! ```
//!
//! **Mix** — full scripting language with AMP integration:
//!
//! ```mix
//! -- @script Toggle Editor Wrap
//! -- @shortcut Ctrl+Shift+W
//! if port_exists("edit") then
//!     address("edit")
//!     send("ui.get", {id: "edit.word-wrap"})
//! end
//! ```
//!
//! # Usage in apps
//!
//! ```ignore
//! // Add User menu (requires "menu" feature)
//! let user_menu = cosmix_script::user_menu("edit");
//!
//! // Handle script actions (TOML or Mix)
//! cosmix_script::handle_script_action(&id, "edit", hub, &vars).await;
//! ```

pub mod types;
pub mod discovery;
pub mod variables;
pub mod executor;

#[cfg(feature = "mix")]
pub mod mix_runtime;

#[cfg(feature = "menu")]
pub mod menu;

// Re-exports for convenience
pub use types::{Script, ScriptDef, ScriptStep, ScriptMeta, ScriptContext, ScriptResult};
pub use discovery::{scripts_dir, discover_scripts};
pub use executor::{execute, execute_script};

#[cfg(feature = "mix")]
pub use mix_runtime::execute_mix;

#[cfg(feature = "menu")]
pub use menu::{user_menu, handle_script_action};
