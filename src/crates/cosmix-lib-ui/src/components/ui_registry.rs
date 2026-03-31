//! AMP-addressable UI element registry and wrapper components.
//!
//! Apps register interactive UI elements by using wrapper components like
//! `AmpButton`, `AmpToggle`, `AmpInput` instead of raw HTML elements.
//! These auto-register into a per-app `UiRegistry` on mount and deregister
//! on unmount.
//!
//! External AMP commands (`ui.invoke`, `ui.highlight`, `ui.list`, `ui.get`,
//! `ui.set`) can then address elements by their semantic ID.

use std::collections::HashMap;
use dioxus::prelude::*;

// ── Types ────────────────────────────────────────────────────────────────

/// What kind of UI element this is (for discovery via `ui.list`).
#[derive(Clone, Debug, PartialEq)]
pub enum ElementKind {
    Button,
    Toggle,
    Input,
}

impl ElementKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Button => "button",
            Self::Toggle => "toggle",
            Self::Input => "input",
        }
    }
}

/// Current state of a registered UI element, reported by `ui.list` and `ui.get`.
#[derive(Clone, Debug, PartialEq)]
pub enum ElementState {
    Button { disabled: bool },
    Toggle { checked: bool, disabled: bool },
    Input { value: String, disabled: bool },
}

impl ElementState {
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Self::Button { disabled } => serde_json::json!({ "disabled": disabled }),
            Self::Toggle { checked, disabled } => serde_json::json!({ "checked": checked, "disabled": disabled }),
            Self::Input { value, disabled } => serde_json::json!({ "value": value, "disabled": disabled }),
        }
    }
}

/// Command that can be sent to a UI element via AMP.
#[derive(Clone, Debug, PartialEq)]
pub enum UiCommand {
    /// Activate the element (click a button, toggle a toggle).
    Invoke,
    /// Visual pulse highlight.
    Highlight { duration_ms: u32 },
    /// Set a value (for inputs: text, for toggles: "true"/"false").
    SetValue(String),
}

/// A registered UI element in the registry.
#[derive(Clone, Debug)]
pub struct UiElement {
    pub id: String,
    pub kind: ElementKind,
    pub label: String,
    pub state: ElementState,
}

/// Per-app registry of AMP-addressable UI elements.
#[derive(Clone, Debug, Default)]
pub struct UiRegistry {
    pub elements: HashMap<String, UiElement>,
}

impl UiRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, element: UiElement) {
        self.elements.insert(element.id.clone(), element);
    }

    pub fn deregister(&mut self, id: &str) {
        self.elements.remove(id);
    }

    /// Update state for an existing element.
    pub fn update_state(&mut self, id: &str, state: ElementState) {
        if let Some(el) = self.elements.get_mut(id) {
            el.state = state;
        }
    }

    /// List all elements, optionally filtered by ID prefix. Includes live state.
    pub fn list(&self, prefix: Option<&str>) -> Vec<serde_json::Value> {
        let mut items: Vec<_> = self.elements.values()
            .filter(|e| prefix.map_or(true, |p| e.id.starts_with(p)))
            .collect();
        items.sort_by(|a, b| a.id.cmp(&b.id));
        items.iter().map(|e| {
            let mut v = serde_json::json!({
                "id": e.id,
                "kind": e.kind.as_str(),
                "label": e.label,
            });
            // Merge state fields into the top-level object
            if let serde_json::Value::Object(state) = e.state.to_json() {
                if let serde_json::Value::Object(ref mut obj) = v {
                    obj.extend(state);
                }
            }
            v
        }).collect()
    }

    /// Get state of specific elements by ID. Supports single or multiple IDs.
    pub fn get(&self, ids: &[&str]) -> Vec<serde_json::Value> {
        ids.iter().filter_map(|id| {
            self.elements.get(*id).map(|e| {
                let mut v = serde_json::json!({
                    "id": e.id,
                    "kind": e.kind.as_str(),
                    "label": e.label,
                });
                if let serde_json::Value::Object(state) = e.state.to_json() {
                    if let serde_json::Value::Object(ref mut obj) = v {
                        obj.extend(state);
                    }
                }
                v
            })
        }).collect()
    }
}

// ── Global signals ───────────────────────────────────────────────────────

/// Write to this signal to send a command to a specific UI element.
pub static UI_CMD: GlobalSignal<Option<(String, UiCommand)>> = Signal::global(|| None);

/// Global UI registry — all Amp* components register themselves here on mount.
pub static UI_REGISTRY: GlobalSignal<UiRegistry> = Signal::global(UiRegistry::new);

// ── Shared CSS ───────────────────────────────────────────────────────────

const AMP_CSS: &str = r#"
/* ── AmpButton ── */
.cmx-amp-btn {
    padding: 4px 12px;
    border: 1px solid var(--border, #374151);
    border-radius: 4px;
    background: var(--bg-secondary, #111827);
    color: var(--fg-primary, #f3f4f6);
    cursor: pointer;
    font-size: var(--font-size-sm, 12px);
    font-family: system-ui, sans-serif;
    transition: background 0.15s;
}
.cmx-amp-btn:hover { background: var(--bg-tertiary, #1f2937); }
.cmx-amp-btn:disabled { opacity: 0.4; cursor: default; pointer-events: none; }

/* ── AmpToggle ── */
.cmx-amp-toggle {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    cursor: pointer;
    font-size: var(--font-size-sm, 12px);
    font-family: system-ui, sans-serif;
    color: var(--fg-primary, #f3f4f6);
    user-select: none;
}
.cmx-amp-toggle input[type="checkbox"] {
    width: 14px; height: 14px;
    accent-color: var(--accent, #3b82f6);
    cursor: pointer;
}
.cmx-amp-toggle.cmx-disabled { opacity: 0.4; pointer-events: none; }

/* ── AmpInput ── */
.cmx-amp-input {
    padding: 4px 8px;
    border: 1px solid var(--border, #374151);
    border-radius: 4px;
    background: var(--bg-primary, #030712);
    color: var(--fg-primary, #f3f4f6);
    font-size: var(--font-size-sm, 12px);
    font-family: system-ui, sans-serif;
    outline: none;
    transition: border-color 0.15s;
}
.cmx-amp-input:focus { border-color: var(--accent, #3b82f6); }
.cmx-amp-input:disabled { opacity: 0.4; }

/* ── Shared highlight pulse ── */
.cmx-amp-highlight { animation: amp-widget-pulse 400ms ease-out; }
@keyframes amp-widget-pulse {
    0%   { box-shadow: 0 0 0 2px var(--accent, #3b82f6); }
    100% { box-shadow: 0 0 0 0 transparent; }
}
"#;

// ── Helper: AMP command watcher ──────────────────────────────────────────

/// Watch UI_CMD for commands targeting this element. Handles highlight animation
/// and calls `on_command` directly for invoke/set commands.
#[cfg(all(not(target_arch = "wasm32"), feature = "hub", feature = "config"))]
fn use_amp_command_watcher(
    id: String,
    on_command: impl Fn(&UiCommand) + 'static,
) -> Signal<bool> {
    let mut is_highlighted = use_signal(|| false);

    use_effect(move || {
        let cmd = UI_CMD.read().clone();
        if let Some((ref target_id, ref command)) = cmd {
            if *target_id == id {
                *UI_CMD.write() = None;

                // Flash highlight for all commands
                is_highlighted.set(true);
                let ms = match command {
                    UiCommand::Highlight { duration_ms } => *duration_ms as u64,
                    _ => 300,
                };
                spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                    is_highlighted.set(false);
                });

                // Dispatch non-highlight commands to the callback
                match command {
                    UiCommand::Highlight { .. } => {}
                    cmd => on_command(cmd),
                }
            }
        }
    });

    is_highlighted
}

// ── AmpButton ────────────────────────────────────────────────────────────

/// AMP-addressable button. Auto-registers with `UiRegistry`.
///
/// ```ignore
/// AmpButton { id: "file.save", label: "Save", on_click: move |_| do_save() }
/// ```
#[component]
pub fn AmpButton(
    id: String,
    label: String,
    on_click: EventHandler<()>,
    #[props(default = false)]
    disabled: bool,
    #[props(default = String::new())]
    class: String,
) -> Element {
    let label_clone = label.clone();

    // Register/update state on every render
    UI_REGISTRY.write().register(UiElement {
        id: id.clone(),
        kind: ElementKind::Button,
        label: label.clone(),
        state: ElementState::Button { disabled },
    });

    use_drop({
        let id = id.clone();
        move || { UI_REGISTRY.write().deregister(&id); }
    });

    // AMP command watching
    #[allow(unused_variables)]
    let is_highlighted = use_signal(|| false);

    #[cfg(all(not(target_arch = "wasm32"), feature = "hub", feature = "config"))]
    let is_highlighted = use_amp_command_watcher(id.clone(), move |cmd| {
        match cmd {
            UiCommand::Invoke | UiCommand::SetValue(_) => on_click.call(()),
            _ => {}
        }
    });

    let highlight_class = if *is_highlighted.read() { " cmx-amp-highlight" } else { "" };
    let extra = if class.is_empty() { String::new() } else { format!(" {class}") };
    let full_class = format!("cmx-amp-btn{highlight_class}{extra}");

    rsx! {
        document::Style { {AMP_CSS} }
        button {
            class: "{full_class}",
            disabled,
            onclick: move |_| on_click.call(()),
            "{label_clone}"
        }
    }
}

// ── AmpToggle ────────────────────────────────────────────────────────────

/// AMP-addressable toggle/checkbox. Auto-registers with `UiRegistry`.
///
/// ```ignore
/// AmpToggle {
///     id: "view.word-wrap",
///     label: "Word Wrap",
///     checked: word_wrap(),
///     on_change: move |v: bool| word_wrap.set(v),
/// }
/// ```
#[component]
pub fn AmpToggle(
    id: String,
    label: String,
    checked: bool,
    on_change: EventHandler<bool>,
    #[props(default = false)]
    disabled: bool,
    #[props(default = String::new())]
    class: String,
) -> Element {
    let label_clone = label.clone();

    // Register/update state on every render (checked changes with the signal)
    UI_REGISTRY.write().register(UiElement {
        id: id.clone(),
        kind: ElementKind::Toggle,
        label: label.clone(),
        state: ElementState::Toggle { checked, disabled },
    });

    use_drop({
        let id = id.clone();
        move || { UI_REGISTRY.write().deregister(&id); }
    });

    // AMP command watching
    #[allow(unused_variables)]
    let is_highlighted = use_signal(|| false);

    #[cfg(all(not(target_arch = "wasm32"), feature = "hub", feature = "config"))]
    let is_highlighted = use_amp_command_watcher(id.clone(), move |cmd| {
        match cmd {
            UiCommand::Invoke => on_change.call(!checked),
            UiCommand::SetValue(v) => {
                let new_val = v == "true" || v == "1";
                on_change.call(new_val);
            }
            _ => {}
        }
    });

    let highlight_class = if *is_highlighted.read() { " cmx-amp-highlight" } else { "" };
    let disabled_class = if disabled { " cmx-disabled" } else { "" };
    let extra = if class.is_empty() { String::new() } else { format!(" {class}") };
    let full_class = format!("cmx-amp-toggle{highlight_class}{disabled_class}{extra}");

    rsx! {
        label {
            class: "{full_class}",
            input {
                r#type: "checkbox",
                checked,
                disabled,
                onchange: move |e: Event<FormData>| {
                    on_change.call(e.checked());
                },
            }
            "{label_clone}"
        }
    }
}

// ── AmpInput ─────────────────────────────────────────────────────────────

/// AMP-addressable text input. Auto-registers with `UiRegistry`.
///
/// ```ignore
/// AmpInput {
///     id: "search.query",
///     label: "Search",
///     value: query(),
///     on_change: move |v: String| query.set(v),
/// }
/// ```
#[component]
pub fn AmpInput(
    id: String,
    label: String,
    value: String,
    on_change: EventHandler<String>,
    #[props(default = false)]
    disabled: bool,
    #[props(default = String::new())]
    placeholder: String,
    #[props(default = String::new())]
    class: String,
) -> Element {
    // Register/update state on every render (value changes with the signal)
    UI_REGISTRY.write().register(UiElement {
        id: id.clone(),
        kind: ElementKind::Input,
        label: label.clone(),
        state: ElementState::Input { value: value.clone(), disabled },
    });

    use_drop({
        let id = id.clone();
        move || { UI_REGISTRY.write().deregister(&id); }
    });

    // AMP command watching
    #[allow(unused_variables)]
    let is_highlighted = use_signal(|| false);

    #[cfg(all(not(target_arch = "wasm32"), feature = "hub", feature = "config"))]
    let is_highlighted = use_amp_command_watcher(id.clone(), move |cmd| {
        match cmd {
            UiCommand::SetValue(v) => on_change.call(v.clone()),
            _ => {}
        }
    });

    let highlight_class = if *is_highlighted.read() { " cmx-amp-highlight" } else { "" };
    let extra = if class.is_empty() { String::new() } else { format!(" {class}") };
    let full_class = format!("cmx-amp-input{highlight_class}{extra}");

    rsx! {
        input {
            class: "{full_class}",
            r#type: "text",
            value,
            disabled,
            placeholder,
            title: "{label}",
            oninput: move |e: Event<FormData>| {
                on_change.call(e.value());
            },
        }
    }
}
