//! cosmix-lib-script — ARexx-style inter-app scripting for cosmix.
//!
//! Provides script discovery, TOML-based script definitions, variable
//! substitution, sequential AMP command execution, and dynamic "User"
//! menu generation.
//!
//! # Script format
//!
//! Scripts are TOML files in `~/.config/cosmix/scripts/{service}/`:
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
//! # Usage in apps
//!
//! ```ignore
//! // Add User menu (requires "menu" feature)
//! let user_menu = cosmix_script::user_menu("edit");
//!
//! // Handle script actions
//! cosmix_script::handle_script_action(&id, "edit", &hub, &vars).await;
//! ```

pub mod types;
pub mod discovery;
pub mod variables;
pub mod executor;

#[cfg(feature = "menu")]
pub mod menu;

// Re-exports for convenience
pub use types::{ScriptDef, ScriptStep, ScriptMeta, ScriptContext, ScriptResult};
pub use discovery::{scripts_dir, discover_scripts};
pub use executor::execute;

#[cfg(feature = "menu")]
pub use menu::{user_menu, handle_script_action};
