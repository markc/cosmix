use dioxus::prelude::*;

use super::types::{MenuAction, MenuBarDef, MenuCommand, MenuItem, SLOT_REGISTRY};

#[cfg(feature = "hub")]
use std::sync::Arc;
#[cfg(feature = "hub")]
use cosmix_client::HubClient;

// ── Global signals for AMP menu control ──────────────────────────────────

/// Write to this signal to send a command to the active MenuBar.
pub static MENU_CMD: GlobalSignal<Option<MenuCommand>> = Signal::global(|| None);

/// Set this to your app's MenuBarDef so `menu.list` can discover items.
pub static MENU_DEF: GlobalSignal<Option<MenuBarDef>> = Signal::global(|| None);

// ── CSS ───────────────────────────────────────────────────────────────────
//
// Uses CSS custom properties with sensible fallback values. The variables
// are injection points for cosmix-confd (future AMP-driven global theming).
// Fallbacks are Tailwind gray-900 palette so the menu renders correctly
// even without any theme injection.

const MENU_CSS: &str = r#"
.cmx-menubar {
    display: flex;
    align-items: center;
    height: 2rem;
    background: var(--bg-secondary, #111827);
    border-bottom: 1px solid var(--border, #1f2937);
    user-select: none;
    flex-shrink: 0;
    font-family: system-ui, sans-serif;
    font-size: 1rem;
    position: relative;
}
.cmx-menu-trigger {
    padding: 0.125rem 0.625rem;
    cursor: pointer;
    font-size: 0.8125rem;
    color: var(--fg-secondary, #e5e7eb);
    border-radius: 0.1875rem;
    height: 1.625rem;
    display: flex;
    align-items: center;
}
.cmx-menu-trigger:hover,
.cmx-menu-trigger.cmx-open {
    background: var(--bg-tertiary, #1f2937);
}
.cmx-dropdown {
    position: fixed;
    min-width: 11.25rem;
    background: var(--bg-secondary, #111827);
    border: 1px solid var(--border, #374151);
    border-radius: 0.25rem;
    box-shadow: 0 0.25rem 1rem rgba(0,0,0,0.6);
    z-index: 9999;
    padding: 0.25rem 0;
}
.cmx-menu-item {
    padding: 0.25rem 2rem 0.25rem 0.75rem;
    cursor: pointer;
    font-size: 0.75rem;
    color: var(--fg-primary, #f3f4f6);
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 1.5rem;
    white-space: nowrap;
}
.cmx-menu-item:hover {
    background: var(--bg-tertiary, #1f2937);
}
.cmx-menu-item.cmx-disabled {
    opacity: 0.4;
    pointer-events: none;
    cursor: default;
}
.cmx-shortcut {
    color: var(--fg-muted, #6b7280);
    font-size: 0.6875rem;
    flex-shrink: 0;
}
.cmx-sep {
    height: 1px;
    background: var(--border, #374151);
    margin: 0.25rem 0;
}
.cmx-overlay {
    position: fixed;
    inset: 0;
    z-index: 9998;
}
.cmx-amp-highlight {
    background: var(--bg-tertiary, #1f2937);
    animation: amp-pulse 400ms ease-out;
}
@keyframes amp-pulse {
    0%   { box-shadow: inset 0 0 0 2px var(--accent, #3b82f6); }
    100% { box-shadow: inset 0 0 0 0 transparent; }
}
/* Drag region — spacer between menus and caption buttons */
.cmx-drag-region {
    flex: 1;
    height: 100%;
}
/* Caption buttons — CSD for frameless windows */
.cmx-caption-btns {
    display: flex;
    align-items: center;
    height: 100%;
}
.cmx-caption-btn {
    width: 2.25rem;
    height: 2rem;
    display: flex;
    align-items: center;
    justify-content: center;
    background: none;
    border: none;
    cursor: pointer;
    color: var(--fg-secondary, #e5e7eb);
    padding: 0;
}
.cmx-caption-btn:hover {
    background: var(--bg-tertiary, #1f2937);
}
.cmx-caption-btn.cmx-close:hover {
    background: var(--danger, #ef4444);
    color: #fff;
}
.cmx-caption-btn svg {
    width: 0.75rem;
    height: 0.75rem;
}
/* App icon (top-left, balances close button on top-right) */
.cmx-app-icon {
    width: 1.75rem;
    height: 2rem;
    display: flex;
    align-items: center;
    justify-content: center;
    flex-shrink: 0;
    padding: 0.25rem;
    color: var(--accent, #60a5fa);
}
.cmx-app-icon svg {
    width: 1.125rem;
    height: 1.125rem;
}
"#;

// ── Caption button icons ─────────────────────────────────────────────────

const ICON_MINIMIZE: &str = r#"<svg viewBox="0 0 12 12" fill="none" stroke="currentColor" stroke-width="1.5"><line x1="2" y1="6" x2="10" y2="6"/></svg>"#;
const ICON_MAXIMIZE: &str = r#"<svg viewBox="0 0 12 12" fill="none" stroke="currentColor" stroke-width="1.5"><rect x="2" y="2" width="8" height="8" rx="1"/></svg>"#;
const ICON_RESTORE: &str = r#"<svg viewBox="0 0 12 12" fill="none" stroke="currentColor" stroke-width="1.5"><rect x="3" y="3" width="7" height="7" rx="1"/><path d="M3 7V3a1 1 0 0 1 1-1h4"/></svg>"#;
const ICON_CLOSE: &str = r#"<svg viewBox="0 0 12 12" fill="none" stroke="currentColor" stroke-width="1.5"><line x1="3" y1="3" x2="9" y2="9"/><line x1="9" y1="3" x2="3" y2="9"/></svg>"#;

// ── Props ─────────────────────────────────────────────────────────────────

#[cfg(feature = "hub")]
#[derive(Props, Clone, PartialEq)]
pub struct MenuBarProps {
    pub menu: MenuBarDef,
    pub on_action: EventHandler<String>,
    #[props(default)]
    pub hub: Option<Signal<Option<Arc<HubClient>>>>,
    /// Optional app icon element for the top-left corner. If not provided,
    /// a default cosmix icon is shown.
    #[props(default)]
    pub icon: Option<Element>,
}

#[cfg(not(feature = "hub"))]
#[derive(Props, Clone, PartialEq)]
pub struct MenuBarProps {
    pub menu: MenuBarDef,
    pub on_action: EventHandler<String>,
    /// Optional app icon element for the top-left corner. If not provided,
    /// a default cosmix icon is shown.
    #[props(default)]
    pub icon: Option<Element>,
}

// ── Component ─────────────────────────────────────────────────────────────

/// Horizontal menu bar with integrated caption buttons (minimize/maximize/close)
/// and a draggable region for frameless windows.
///
/// On desktop, apps use `cosmix_ui::desktop::window_config()` which sets
/// `with_decorations(false)` — the MenuBar provides all window chrome (CSD).
/// This avoids any dependency on specific Wayland compositors.
///
/// On WASM, caption buttons provide fullscreen toggle and tab close.
/// The same MenuBar works identically in both environments.
#[component]
pub fn MenuBar(props: MenuBarProps) -> Element {
    // Index of currently open top-level menu, None = all closed
    let mut open_idx: Signal<Option<usize>> = use_signal(|| None);
    // Position of the open dropdown (left, top in pixels)
    let mut drop_pos: Signal<(f64, f64)> = use_signal(|| (0.0, 0.0));
    // ID of the menu item currently highlighted by an AMP command
    #[allow(unused_mut)]
    let mut highlight_id: Signal<Option<String>> = use_signal(|| None);

    let menu = props.menu.clone();
    let app_icon = props.icon;
    let on_action = props.on_action.clone();
    #[cfg(feature = "hub")]
    let hub = props.hub.clone();

    // ── AMP menu command processing (requires hub + config for tokio sleep) ──
    #[cfg(all(not(target_arch = "wasm32"), feature = "hub", feature = "config"))]
    {
        let menu2 = menu.clone();
        let on_action2 = on_action.clone();
        #[cfg(feature = "hub")]
        let hub2 = hub.clone();
        use_effect(move || {
            let cmd = MENU_CMD.read().clone();
            let Some(cmd) = cmd else { return };
            // Consume the command immediately
            *MENU_CMD.write() = None;

            match cmd {
                MenuCommand::Close => {
                    open_idx.set(None);
                    highlight_id.set(None);
                }
                MenuCommand::Highlight { id, duration_ms } => {
                    if let Some((idx, _)) = menu2.find_item(&id) {
                        // Open the parent menu at a default position
                        drop_pos.set((10.0 + idx as f64 * 60.0, 28.0));
                        open_idx.set(Some(idx));
                        highlight_id.set(Some(id.clone()));
                        // Clear highlight after duration
                        let ms = duration_ms;
                        spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_millis(ms as u64)).await;
                            highlight_id.set(None);
                        });
                    }
                }
                MenuCommand::Invoke { id } => {
                    if let Some((idx, item)) = menu2.find_item(&id) {
                        // Open menu and highlight briefly
                        drop_pos.set((10.0 + idx as f64 * 60.0, 28.0));
                        open_idx.set(Some(idx));
                        highlight_id.set(Some(id.clone()));

                        // Fire the action through the normal dispatch path
                        if let MenuItem::Action { ref action, .. } = item {
                            #[cfg(feature = "hub")]
                            dispatch_amp_action(action, &on_action2, &hub2);
                            #[cfg(not(feature = "hub"))]
                            dispatch_local_action(action, &on_action2);
                        }

                        // Clear highlight and close menu after a brief delay
                        spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                            highlight_id.set(None);
                            open_idx.set(None);
                        });
                    }
                }
            }
        });
    }

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

            // App icon (top-left)
            div { class: "cmx-app-icon",
                if let Some(icon) = app_icon {
                    {icon}
                } else {
                    {cosmix_icon()}
                }
            }

            // Resolve slots: flatten MenuItem::Slot into their injected entries.
            // Reading SLOT_REGISTRY creates a reactive subscription — when slots
            // change, the menu bar re-renders automatically.
            for (idx, top_item) in resolve_menu_slots(&menu.menus).iter().enumerate() {
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
                                onmouseenter: move |e: MouseEvent| {
                                    if open_idx.read().is_some() {
                                        let coords = e.client_coordinates();
                                        drop_pos.set((coords.x, 28.0));
                                        open_idx.set(Some(idx));
                                    }
                                },
                                "{label}"
                            }

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
                                                        highlight_id,
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

            // Draggable spacer + caption buttons
            {drag_region()}
            {caption_buttons()}
        }
    }
}

// ── Drag region ───────────────────────────────────────────────────────────

#[cfg(feature = "desktop")]
fn drag_region() -> Element {
    // Track maximized state for double-click toggle
    let mut is_maximized = use_signal(|| false);

    rsx! {
        div {
            class: "cmx-drag-region",
            onmousedown: move |_| {
                let window = dioxus_desktop::use_window();
                let _ = window.drag_window();
            },
            ondoubleclick: move |_| {
                let window = dioxus_desktop::use_window();
                let max = *is_maximized.read();
                window.set_maximized(!max);
                is_maximized.set(!max);
            },
        }
    }
}

#[cfg(not(feature = "desktop"))]
fn drag_region() -> Element {
    // No drag on WASM — just a spacer, but double-click toggles fullscreen
    rsx! {
        div {
            class: "cmx-drag-region",
            ondoubleclick: move |_| {
                document::eval(r#"
                    if (document.fullscreenElement) {
                        document.exitFullscreen();
                    } else {
                        document.documentElement.requestFullscreen();
                    }
                "#);
            },
        }
    }
}

// ── Caption buttons ───────────────────────────────────────────────────────

#[cfg(feature = "desktop")]
fn caption_buttons() -> Element {
    let mut is_maximized = use_signal(|| false);
    let maximize_icon = if *is_maximized.read() { ICON_RESTORE } else { ICON_MAXIMIZE };
    let maximize_title = if *is_maximized.read() { "Restore" } else { "Maximize" };

    rsx! {
        div { class: "cmx-caption-btns",
            button {
                class: "cmx-caption-btn",
                title: "Minimize",
                onclick: move |_| {
                    let window = dioxus_desktop::use_window();
                    window.set_minimized(true);
                },
                span { dangerous_inner_html: ICON_MINIMIZE }
            }
            button {
                class: "cmx-caption-btn",
                title: "{maximize_title}",
                onclick: move |_| {
                    let window = dioxus_desktop::use_window();
                    let max = *is_maximized.read();
                    window.set_maximized(!max);
                    is_maximized.set(!max);
                },
                span { dangerous_inner_html: maximize_icon }
            }
            button {
                class: "cmx-caption-btn cmx-close",
                title: "Close",
                onclick: move |_| {
                    let window = dioxus_desktop::use_window();
                    window.close();
                },
                span { dangerous_inner_html: ICON_CLOSE }
            }
        }
    }
}

#[cfg(not(feature = "desktop"))]
fn caption_buttons() -> Element {
    let mut is_fullscreen = use_signal(|| false);
    let maximize_icon = if *is_fullscreen.read() { ICON_RESTORE } else { ICON_MAXIMIZE };
    let maximize_title = if *is_fullscreen.read() { "Exit Fullscreen" } else { "Fullscreen" };

    rsx! {
        div { class: "cmx-caption-btns",
            // Minimize → hide the shell UI, show a minimal restore bar
            button {
                class: "cmx-caption-btn",
                title: "Minimize",
                onclick: move |_| {
                    // Minimize the browser window (works if opened by script)
                    document::eval("window.blur(); if (window.opener) window.minimize?.();");
                },
                span { dangerous_inner_html: ICON_MINIMIZE }
            }
            // Maximize → toggle browser fullscreen
            button {
                class: "cmx-caption-btn",
                title: "{maximize_title}",
                onclick: move |_| {
                    let fs = *is_fullscreen.read();
                    if fs {
                        document::eval("document.exitFullscreen().catch(()=>{});");
                    } else {
                        document::eval("document.documentElement.requestFullscreen().catch(()=>{});");
                    }
                    is_fullscreen.set(!fs);
                },
                span { dangerous_inner_html: maximize_icon }
            }
            // Close → close tab (works if opened by script, otherwise no-op)
            button {
                class: "cmx-caption-btn cmx-close",
                title: "Close",
                onclick: move |_| {
                    document::eval("window.close();");
                },
                span { dangerous_inner_html: ICON_CLOSE }
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
    highlight_id: Signal<Option<String>>,
) -> Element {
    render_item_inner(item, on_action, Some(hub), open_idx, highlight_id)
}

#[cfg(not(feature = "hub"))]
fn render_item(
    item: &MenuItem,
    on_action: EventHandler<String>,
    open_idx: Signal<Option<usize>>,
    highlight_id: Signal<Option<String>>,
) -> Element {
    render_item_inner(item, on_action, open_idx, highlight_id)
}

#[cfg(feature = "hub")]
fn render_item_inner(
    item: &MenuItem,
    on_action: EventHandler<String>,
    hub: Option<Option<Signal<Option<Arc<HubClient>>>>>,
    open_idx: Signal<Option<usize>>,
    highlight_id: Signal<Option<String>>,
) -> Element {
    render_item_shared(item, on_action, hub.flatten(), open_idx, highlight_id)
}

#[cfg(not(feature = "hub"))]
fn render_item_inner(
    item: &MenuItem,
    on_action: EventHandler<String>,
    open_idx: Signal<Option<usize>>,
    highlight_id: Signal<Option<String>>,
) -> Element {
    render_item_shared(item, on_action, open_idx, highlight_id)
}

#[cfg(feature = "hub")]
fn render_item_shared(
    item: &MenuItem,
    on_action: EventHandler<String>,
    hub: Option<Signal<Option<Arc<HubClient>>>>,
    mut open_idx: Signal<Option<usize>>,
    highlight_id: Signal<Option<String>>,
) -> Element {
    match item {
        MenuItem::Separator => rsx! { div { class: "cmx-sep" } },
        MenuItem::Action { id, label, shortcut, action, enabled } => {
            let label = label.clone();
            let shortcut_label = shortcut.as_ref().map(|s| s.label());
            let action = action.clone();
            let is_highlighted = highlight_id.read().as_deref() == Some(id.as_str());
            let class = match (*enabled, is_highlighted) {
                (false, _)    => "cmx-menu-item cmx-disabled",
                (true, true)  => "cmx-menu-item cmx-amp-highlight",
                (true, false) => "cmx-menu-item",
            };

            rsx! {
                div {
                    class,
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
            rsx! {
                div { class: "cmx-menu-item cmx-disabled",
                    span { "{label}" }
                    span { class: "cmx-shortcut", "▶" }
                }
            }
        }
        MenuItem::Slot { .. } => rsx! {},
    }
}

#[cfg(not(feature = "hub"))]
fn render_item_shared(
    item: &MenuItem,
    on_action: EventHandler<String>,
    mut open_idx: Signal<Option<usize>>,
    highlight_id: Signal<Option<String>>,
) -> Element {
    match item {
        MenuItem::Separator => rsx! { div { class: "cmx-sep" } },
        MenuItem::Action { id, label, shortcut, action, enabled } => {
            let label = label.clone();
            let shortcut_label = shortcut.as_ref().map(|s| s.label());
            let action = action.clone();
            let is_highlighted = highlight_id.read().as_deref() == Some(id.as_str());
            let class = match (*enabled, is_highlighted) {
                (false, _)    => "cmx-menu-item cmx-disabled",
                (true, true)  => "cmx-menu-item cmx-amp-highlight",
                (true, false) => "cmx-menu-item",
            };

            rsx! {
                div {
                    class,
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
        MenuItem::Slot { .. } => rsx! {},
    }
}

// ── Default app icon ─────────────────────────────────────────────────────

/// Render the Cosmix brand icon as a native Dioxus SVG element.
fn cosmix_icon() -> Element {
    rsx! {
        svg {
            width: "18",
            height: "18",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.5",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            path { d: "M21 16V8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73l7 4a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16z" }
            path { d: "M15 9.4a4 4 0 1 0 0 5.2" }
        }
    }
}

// ── Slot resolution ──────────────────────────────────────────────────────

/// Resolve `MenuItem::Slot` variants by looking up `SLOT_REGISTRY` and
/// replacing each slot with its injected entries. Non-slot items pass through.
/// Reading `SLOT_REGISTRY` creates a Dioxus reactive subscription.
fn resolve_menu_slots(menus: &[MenuItem]) -> Vec<MenuItem> {
    let registry = SLOT_REGISTRY.read();
    menus.iter().flat_map(|item| {
        match item {
            MenuItem::Slot { name } => {
                registry.resolve(name).iter().map(|e| e.item.clone()).collect::<Vec<_>>()
            }
            other => vec![other.clone()],
        }
    }).collect()
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
