//! Core types for script definitions, execution context, and results.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A discovered script — either a TOML step-sequence or a Mix program.
#[derive(Debug, Clone)]
pub enum Script {
    /// TOML-defined AMP step sequence.
    Toml(ScriptDef),
    /// Mix source file with metadata from comment headers.
    Mix { meta: ScriptMeta, path: PathBuf },
}

impl Script {
    /// Get the script metadata regardless of type.
    pub fn meta(&self) -> &ScriptMeta {
        match self {
            Script::Toml(def) => &def.script,
            Script::Mix { meta, .. } => meta,
        }
    }
}

/// A complete script definition parsed from a TOML file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptDef {
    pub script: ScriptMeta,
    #[serde(default)]
    pub steps: Vec<ScriptStep>,
}

/// Script metadata — name, optional shortcut, description.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptMeta {
    /// Display name in the User menu.
    pub name: String,
    /// Optional keyboard shortcut (e.g. "Ctrl+Shift+V").
    #[serde(default)]
    pub shortcut: Option<String>,
    /// Human-readable description (shown in tooltip or help).
    #[serde(default)]
    pub description: Option<String>,
}

/// A single step in a script — sends an AMP command to a service.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptStep {
    /// Target service name (e.g. "view", "edit", "mon", or "edit.mko.amp" for mesh).
    pub to: String,
    /// AMP command (e.g. "view.open", "edit.get-content").
    pub command: String,
    /// JSON string with `$VAR` placeholders. Substituted at runtime.
    #[serde(default)]
    pub args: Option<String>,
    /// Store the response body in a named variable for subsequent steps.
    #[serde(default)]
    pub store: Option<String>,
}

/// Runtime context for variable substitution during script execution.
pub struct ScriptContext {
    /// App-provided variables: `$CURRENT_FILE`, `$SELECTION`, `$LINE`, etc.
    pub app_vars: HashMap<String, String>,
    /// Variables stored by previous steps via `store = "name"`.
    pub step_vars: HashMap<String, String>,
    /// The service name of the app running this script.
    pub service_name: String,
}

impl ScriptContext {
    pub fn new(service_name: &str, app_vars: HashMap<String, String>) -> Self {
        Self {
            app_vars,
            step_vars: HashMap::new(),
            service_name: service_name.to_string(),
        }
    }
}

/// Result of executing a script.
pub struct ScriptResult {
    /// AMP return code: 0=success, 5=warning, 10=error, 20=failure.
    pub rc: u8,
    /// Response body from the last successful step.
    pub body: Option<String>,
    /// Error message if execution failed.
    pub error: Option<String>,
}
