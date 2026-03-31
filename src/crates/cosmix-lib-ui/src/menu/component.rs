use dioxus::prelude::*;

use super::types::{MenuAction, MenuBarDef, MenuCommand, MenuItem};

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

const MENU_CSS: &str = r#"
.cmx-menubar {
    display: flex;
    align-items: center;
    height: 28px;
    background: var(--bg-secondary, #111827);
    border-bottom: 1px solid var(--border, #1f2937);
    user-select: none;
    flex-shrink: 0;
    font-family: system-ui, sans-serif;
    position: relative;
}
.cmx-menu-trigger {
    padding: 2px 10px;
    cursor: pointer;
    font-size: var(--font-size-sm, 12px);
    color: var(--fg-secondary, #e5e7eb);
    border-radius: 3px;
    height: 22px;
    display: flex;
    align-items: center;
}
.cmx-menu-trigger:hover,
.cmx-menu-trigger.cmx-open {
    background: var(--bg-tertiary, #1f2937);
}
.cmx-dropdown {
    position: fixed;
    min-width: 180px;
    background: var(--bg-secondary, #111827);
    border: 1px solid var(--border, #374151);
    border-radius: 4px;
    box-shadow: 0 4px 16px rgba(0,0,0,0.6);
    z-index: 9999;
    padding: 4px 0;
}
.cmx-menu-item {
    padding: 4px 32px 4px 12px;
    cursor: pointer;
    font-size: var(--font-size-sm, 12px);
    color: var(--fg-primary, #f3f4f6);
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 24px;
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
    font-size: calc(var(--font-size-sm, 12px) - 1px);
    flex-shrink: 0;
}
.cmx-sep {
    height: 1px;
    background: var(--border, #374151);
    margin: 4px 0;
}
.cmx-overlay {
    position: fixed;
    inset: 0;
    z-index: 9998;
}
/* AMP highlight pulse — applied to menu items targeted by menu.highlight/invoke */
.cmx-amp-highlight {
    background: var(--bg-tertiary, #1f2937);
    animation: amp-pulse 400ms ease-out;
}
@keyframes amp-pulse {
    0%   { box-shadow: inset 0 0 0 2px var(--accent, #3b82f6); }
    100% { box-shadow: inset 0 0 0 0 transparent; }
}
/* Drag region — the spacer area between menus and caption buttons */
.cmx-drag-region {
    flex: 1;
    height: 100%;
}
/* Caption buttons */
.cmx-caption-btns {
    display: flex;
    align-items: center;
    height: 100%;
}
.cmx-caption-btn {
    width: 36px;
    height: 28px;
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
    width: 12px;
    height: 12px;
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
}

#[cfg(not(feature = "hub"))]
#[derive(Props, Clone, PartialEq)]
pub struct MenuBarProps {
    pub menu: MenuBarDef,
    pub on_action: EventHandler<String>,
}

// ── Component ─────────────────────────────────────────────────────────────

/// Horizontal menu bar with integrated caption buttons (minimize/maximize/close)
/// and a draggable region for frameless windows.
///
/// On desktop, apps should use `cosmix_ui::desktop::window_config()` which sets
/// `with_decorations(false)` — the MenuBar replaces the compositor's header bar.
///
/// On WASM, caption buttons are hidden (no native window to control).
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
                        if let MenuItem::Action { action, .. } = item {
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
