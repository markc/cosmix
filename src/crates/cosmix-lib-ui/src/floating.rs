//! Draggable floating panel — works on desktop and WASM.
//!
//! Renders as a `position: fixed` div with a title bar that supports
//! drag-to-move, minimize (collapse), and close. Panels stay inside
//! the WebView bounds on both native and browser targets.
//!
//! # Example
//! ```ignore
//! FloatingPanel {
//!     title: "Tool Palette",
//!     initial_x: 100.0,
//!     initial_y: 100.0,
//!     width: 320.0,
//!     height: 240.0,
//!     on_close: move |_| show_panel.set(false),
//!     div { "Panel content here" }
//! }
//! ```

use dioxus::prelude::*;

const FLOATING_CSS: &str = r#"
.cmx-float {
    position: fixed;
    z-index: 5000;
    display: flex;
    flex-direction: column;
    background: var(--bg-secondary, #111827);
    border: 1px solid var(--border, #374151);
    border-radius: var(--radius-md, 6px);
    box-shadow: 0 8px 32px rgba(0,0,0,0.4);
    overflow: hidden;
}
.cmx-float-titlebar {
    height: 28px;
    display: flex;
    align-items: center;
    padding: 0 8px;
    background: var(--bg-tertiary, #1f2937);
    cursor: grab;
    user-select: none;
    flex-shrink: 0;
    gap: 6px;
}
.cmx-float-titlebar:active {
    cursor: grabbing;
}
.cmx-float-title {
    flex: 1;
    font-size: 12px;
    font-weight: 500;
    color: var(--fg-secondary, #e5e7eb);
    font-family: system-ui, sans-serif;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
}
.cmx-float-btn {
    width: 20px;
    height: 20px;
    display: flex;
    align-items: center;
    justify-content: center;
    background: none;
    border: none;
    cursor: pointer;
    color: var(--fg-muted, #6b7280);
    border-radius: var(--radius-sm, 3px);
    padding: 0;
}
.cmx-float-btn:hover {
    background: var(--bg-secondary, #111827);
    color: var(--fg-primary, #f3f4f6);
}
.cmx-float-btn.cmx-float-close:hover {
    background: var(--danger, #ef4444);
    color: #fff;
}
.cmx-float-body {
    flex: 1;
    overflow: auto;
}
"#;

const ICON_MINIMIZE: &str = r#"<svg viewBox="0 0 10 10" fill="none" stroke="currentColor" stroke-width="1.5"><line x1="2" y1="5" x2="8" y2="5"/></svg>"#;
const ICON_CLOSE: &str = r#"<svg viewBox="0 0 10 10" fill="none" stroke="currentColor" stroke-width="1.5"><line x1="2" y1="2" x2="8" y2="8"/><line x1="8" y1="2" x2="2" y2="8"/></svg>"#;

#[derive(Props, Clone, PartialEq)]
pub struct FloatingPanelProps {
    /// Title shown in the drag bar.
    pub title: String,
    /// Initial X position (pixels from left).
    #[props(default = 100.0)]
    pub initial_x: f64,
    /// Initial Y position (pixels from top).
    #[props(default = 100.0)]
    pub initial_y: f64,
    /// Panel width in pixels.
    #[props(default = 320.0)]
    pub width: f64,
    /// Panel height in pixels (when not minimized).
    #[props(default = 240.0)]
    pub height: f64,
    /// Called when the close button is clicked.
    pub on_close: EventHandler<()>,
    /// Panel body content.
    pub children: Element,
}

/// A draggable floating panel that works identically on desktop and WASM.
#[component]
pub fn FloatingPanel(props: FloatingPanelProps) -> Element {
    let mut pos_x = use_signal(|| props.initial_x);
    let mut pos_y = use_signal(|| props.initial_y);
    let mut minimized = use_signal(|| false);
    let mut dragging = use_signal(|| false);
    let mut drag_offset = use_signal(|| (0.0f64, 0.0f64));

    let x = *pos_x.read();
    let y = *pos_y.read();
    let w = props.width;
    let h = if *minimized.read() { 28.0 } else { props.height };

    let on_close = props.on_close.clone();
    let title = props.title.clone();

    rsx! {
        document::Style { {FLOATING_CSS} }

        // Drag overlay — captures mousemove/mouseup while dragging
        if *dragging.read() {
            div {
                style: "position:fixed; inset:0; z-index:9999; cursor:grabbing;",
                onmousemove: move |e: MouseEvent| {
                    let coords = e.client_coordinates();
                    let (ox, oy) = *drag_offset.read();
                    pos_x.set(coords.x - ox);
                    pos_y.set(coords.y - oy);
                },
                onmouseup: move |_| {
                    dragging.set(false);
                },
            }
        }

        div {
            class: "cmx-float",
            style: "left:{x}px; top:{y}px; width:{w}px; height:{h}px;",

            // Title bar (drag handle)
            div {
                class: "cmx-float-titlebar",
                onmousedown: move |e: MouseEvent| {
                    let coords = e.client_coordinates();
                    drag_offset.set((coords.x - *pos_x.read(), coords.y - *pos_y.read()));
                    dragging.set(true);
                },

                span { class: "cmx-float-title", "{title}" }

                button {
                    class: "cmx-float-btn",
                    title: if *minimized.read() { "Restore" } else { "Minimize" },
                    onclick: move |e: MouseEvent| {
                        e.stop_propagation();
                        let m = *minimized.read();
                        minimized.set(!m);
                    },
                    span { dangerous_inner_html: ICON_MINIMIZE }
                }
                button {
                    class: "cmx-float-btn cmx-float-close",
                    title: "Close",
                    onclick: move |e: MouseEvent| {
                        e.stop_propagation();
                        on_close.call(());
                    },
                    span { dangerous_inner_html: ICON_CLOSE }
                }
            }

            // Body (hidden when minimized)
            if !*minimized.read() {
                div {
                    class: "cmx-float-body",
                    {props.children.clone()}
                }
            }
        }
    }
}
