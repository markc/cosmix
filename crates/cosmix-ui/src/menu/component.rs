use dioxus::prelude::*;

use super::types::{MenuAction, MenuBarDef, MenuItem};

#[cfg(feature = "hub")]
use std::sync::Arc;
#[cfg(feature = "hub")]
use cosmix_client::HubClient;

// ── CSS ───────────────────────────────────────────────────────────────────

const MENU_CSS: &str = r#"
.cmx-menubar {
    display: flex;
    align-items: center;
    height: 28px;
    background: #111827;
    border-bottom: 1px solid #1f2937;
    user-select: none;
    flex-shrink: 0;
    font-family: system-ui, sans-serif;
    position: relative;
}
.cmx-menu-trigger {
    padding: 2px 10px;
    cursor: pointer;
    font-size: 12px;
    color: #e5e7eb;
    border-radius: 3px;
    height: 22px;
    display: flex;
    align-items: center;
}
.cmx-menu-trigger:hover,
.cmx-menu-trigger.cmx-open {
    background: #1f2937;
}
.cmx-dropdown {
    position: fixed;
    min-width: 180px;
    background: #111827;
    border: 1px solid #374151;
    border-radius: 4px;
    box-shadow: 0 4px 16px rgba(0,0,0,0.6);
    z-index: 9999;
    padding: 4px 0;
}
.cmx-menu-item {
    padding: 4px 32px 4px 12px;
    cursor: pointer;
    font-size: 12px;
    color: #f3f4f6;
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 24px;
    white-space: nowrap;
}
.cmx-menu-item:hover {
    background: #1f2937;
}
.cmx-menu-item.cmx-disabled {
    opacity: 0.4;
    pointer-events: none;
    cursor: default;
}
.cmx-shortcut {
    color: #6b7280;
    font-size: 11px;
    flex-shrink: 0;
}
.cmx-sep {
    height: 1px;
    background: #374151;
    margin: 4px 0;
}
.cmx-overlay {
    position: fixed;
    inset: 0;
    z-index: 9998;
}
"#;

// ── Props ─────────────────────────────────────────────────────────────────

#[cfg(feature = "hub")]
#[derive(Props, Clone, PartialEq)]
pub struct MenuBarProps {
    pub menu: MenuBarDef,
    pub on_action: EventHandler<String>,
    #[props(default)]
    pub hub: Option<Signal<Option<Arc<HubClient>>>>,
}

#[cfg(not(feature = "hub"))]
#[derive(Props, Clone, PartialEq)]
pub struct MenuBarProps {
    pub menu: MenuBarDef,
    pub on_action: EventHandler<String>,
}

// ── Component ─────────────────────────────────────────────────────────────

/// Horizontal menu bar — works on both desktop and WASM.
///
/// # Example
/// ```ignore
/// MenuBar {
///     menu: menubar(vec![standard_file_menu(vec![
///         action_shortcut("open", "Open...", Shortcut::ctrl('o')),
///         action_shortcut("save", "Save", Shortcut::ctrl('s')),
///     ])]),
///     on_action: move |id: String| match id.as_str() {
///         "open" => { /* ... */ }
///         "save" => { /* ... */ }
///         "quit" => { std::process::exit(0); }
///         _ => {}
///     },
/// }
/// ```
#[component]
pub fn MenuBar(props: MenuBarProps) -> Element {
    // Index of currently open top-level menu, None = all closed
    let mut open_idx: Signal<Option<usize>> = use_signal(|| None);
    // Position of the open dropdown (left, top in pixels)
    let mut drop_pos: Signal<(f64, f64)> = use_signal(|| (0.0, 0.0));

    let menu = props.menu.clone();
    let on_action = props.on_action.clone();
    #[cfg(feature = "hub")]
    let hub = props.hub.clone();

    rsx! {
        // Inject CSS once
        document::Style { {MENU_CSS} }

        div { class: "cmx-menubar",

            // Transparent overlay to close menus on outside click
            if open_idx.read().is_some() {
                div {
                    class: "cmx-overlay",
                    onclick: move |_| { open_idx.set(None); },
                }
            }

            // Top-level menu triggers
            for (idx, top_item) in menu.menus.iter().enumerate() {
                if let MenuItem::Submenu { label, items } = top_item {
                    {
                        let label = label.clone();
                        let items = items.clone();
                        let is_open = *open_idx.read() == Some(idx);
                        let on_action2 = on_action.clone();
                        #[cfg(feature = "hub")]
                        let hub2 = hub.clone();

                        rsx! {
                            div {
                                class: if is_open { "cmx-menu-trigger cmx-open" } else { "cmx-menu-trigger" },
                                // Open dropdown on click; if already open, close
                                onclick: move |e: MouseEvent| {
                                    e.stop_propagation();
                                    if is_open {
                                        open_idx.set(None);
                                    } else {
                                        let coords = e.client_coordinates();
                                        drop_pos.set((coords.x, 28.0));
                                        open_idx.set(Some(idx));
                                    }
                                },
                                // Hover switches open menu when another is already open
                                onmouseenter: move |e: MouseEvent| {
                                    if open_idx.read().is_some() {
                                        let coords = e.client_coordinates();
                                        drop_pos.set((coords.x, 28.0));
                                        open_idx.set(Some(idx));
                                    }
                                },
                                "{label}"
                            }

                            // Dropdown for this trigger
                            if is_open {
                                {
                                    let (left, top) = *drop_pos.read();
                                    rsx! {
                                        div {
                                            class: "cmx-dropdown",
                                            style: "left:{left}px; top:{top}px;",
                                            onclick: move |e| e.stop_propagation(),
                                            for item in items.iter() {
                                                {
                                                    #[cfg(feature = "hub")]
                                                    let hub3 = hub2.clone();
                                                    render_item(
                                                        item,
                                                        on_action2.clone(),
                                                        #[cfg(feature = "hub")]
                                                        hub3,
                                                        open_idx,
                                                    )
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

// ── Dropdown item renderer ────────────────────────────────────────────────

#[cfg(feature = "hub")]
fn render_item(
    item: &MenuItem,
    on_action: EventHandler<String>,
    hub: Option<Signal<Option<Arc<HubClient>>>>,
    open_idx: Signal<Option<usize>>,
) -> Element {
    render_item_inner(item, on_action, Some(hub), open_idx)
}

#[cfg(not(feature = "hub"))]
fn render_item(
    item: &MenuItem,
    on_action: EventHandler<String>,
    open_idx: Signal<Option<usize>>,
) -> Element {
    render_item_inner(item, on_action, open_idx)
}

#[cfg(feature = "hub")]
fn render_item_inner(
    item: &MenuItem,
    on_action: EventHandler<String>,
    hub: Option<Option<Signal<Option<Arc<HubClient>>>>>,
    open_idx: Signal<Option<usize>>,
) -> Element {
    render_item_shared(item, on_action, hub.flatten(), open_idx)
}

#[cfg(not(feature = "hub"))]
fn render_item_inner(
    item: &MenuItem,
    on_action: EventHandler<String>,
    open_idx: Signal<Option<usize>>,
) -> Element {
    render_item_shared(item, on_action, open_idx)
}

#[cfg(feature = "hub")]
fn render_item_shared(
    item: &MenuItem,
    on_action: EventHandler<String>,
    hub: Option<Signal<Option<Arc<HubClient>>>>,
    mut open_idx: Signal<Option<usize>>,
) -> Element {
    match item {
        MenuItem::Separator => rsx! { div { class: "cmx-sep" } },
        MenuItem::Action { label, shortcut, action, enabled, .. } => {
            let label = label.clone();
            let shortcut_label = shortcut.as_ref().map(|s| s.label());
            let action = action.clone();
            let disabled_class = if *enabled { "cmx-menu-item" } else { "cmx-menu-item cmx-disabled" };

            rsx! {
                div {
                    class: disabled_class,
                    onclick: move |_| {
                        open_idx.set(None);
                        dispatch_amp_action(&action, &on_action, &hub);
                    },
                    span { "{label}" }
                    if let Some(sc) = shortcut_label {
                        span { class: "cmx-shortcut", "{sc}" }
                    }
                }
            }
        }
        MenuItem::Submenu { label, .. } => {
            // Nested submenus not rendered in v1 — show as disabled
            rsx! {
                div { class: "cmx-menu-item cmx-disabled",
                    span { "{label}" }
                    span { class: "cmx-shortcut", "▶" }
                }
            }
        }
    }
}

#[cfg(not(feature = "hub"))]
fn render_item_shared(
    item: &MenuItem,
    on_action: EventHandler<String>,
    mut open_idx: Signal<Option<usize>>,
) -> Element {
    match item {
        MenuItem::Separator => rsx! { div { class: "cmx-sep" } },
        MenuItem::Action { label, shortcut, action, enabled, .. } => {
            let label = label.clone();
            let shortcut_label = shortcut.as_ref().map(|s| s.label());
            let action = action.clone();
            let disabled_class = if *enabled { "cmx-menu-item" } else { "cmx-menu-item cmx-disabled" };

            rsx! {
                div {
                    class: disabled_class,
                    onclick: move |_| {
                        open_idx.set(None);
                        dispatch_local_action(&action, &on_action);
                    },
                    span { "{label}" }
                    if let Some(sc) = shortcut_label {
                        span { class: "cmx-shortcut", "{sc}" }
                    }
                }
            }
        }
        MenuItem::Submenu { label, .. } => {
            rsx! {
                div { class: "cmx-menu-item cmx-disabled",
                    span { "{label}" }
                    span { class: "cmx-shortcut", "▶" }
                }
            }
        }
    }
}

// ── Action dispatch ────────────────────────────────────────────────────────

#[cfg(feature = "hub")]
fn dispatch_amp_action(
    action: &MenuAction,
    on_action: &EventHandler<String>,
    hub: &Option<Signal<Option<Arc<HubClient>>>>,
) {
    match action {
        MenuAction::Local(id) => on_action.call(id.clone()),
        MenuAction::Amp { to, command, args } => {
            if let Some(hub_sig) = hub {
                if let Some(client) = hub_sig.read().as_ref() {
                    let client = client.clone();
                    let to = to.clone();
                    let command = command.clone();
                    let args = args.clone();
                    spawn(async move {
                        if let Err(e) = client.call(&to, &command, args).await {
                            tracing::warn!("Menu AMP action failed ({command}): {e}");
                        }
                    });
                } else {
                    tracing::warn!("Menu AMP action: hub not connected");
                }
            }
        }
        MenuAction::None => {}
    }
}

#[cfg(not(feature = "hub"))]
fn dispatch_local_action(action: &MenuAction, on_action: &EventHandler<String>) {
    match action {
        MenuAction::Local(id) => on_action.call(id.clone()),
        MenuAction::None => {}
    }
}
