use dioxus::prelude::*;

/// A carousel of panels with dot indicators and chevron navigation.
#[component]
pub fn Carousel(
    panels: Vec<String>,
    active: usize,
    on_navigate: EventHandler<usize>,
    children: Element,
) -> Element {
    let count = panels.len();

    rsx! {
        div {
            style: "display:flex; flex-direction:column; height:100%;",

            // Panel content
            div { style: "flex:1; overflow-y:auto;",
                {children}
            }

            // Navigation bar
            div {
                style: "display:flex; align-items:center; justify-content:center; gap:6px; padding:6px 8px; background:var(--bg-tertiary); border-top:1px solid var(--border-muted); flex-shrink:0;",

                // Previous chevron
                button {
                    style: "background:none; border:none; color:var(--fg-muted); cursor:pointer; padding:2px 4px; font-size:12px;",
                    disabled: active == 0,
                    onclick: move |_| {
                        if active > 0 { on_navigate.call(active - 1); }
                    },
                    "<"
                }

                // Dot indicators
                for (i, name) in panels.iter().enumerate() {
                    {
                        let is_active = i == active;
                        let dot_bg = if is_active { "var(--accent)" } else { "var(--border)" };
                        let title = name.clone();
                        rsx! {
                            button {
                                style: "width:8px; height:8px; border-radius:50%; background:{dot_bg}; border:none; cursor:pointer; padding:0;",
                                title: "{title}",
                                onclick: move |_| { on_navigate.call(i); },
                            }
                        }
                    }
                }

                // Next chevron
                button {
                    style: "background:none; border:none; color:var(--fg-muted); cursor:pointer; padding:2px 4px; font-size:12px;",
                    disabled: active >= count.saturating_sub(1),
                    onclick: move |_| {
                        if active + 1 < count { on_navigate.call(active + 1); }
                    },
                    ">"
                }
            }
        }
    }
}
