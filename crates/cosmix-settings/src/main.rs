//! cosmix-settings — GUI settings editor for the cosmix stack.
//!
//! Sidebar lists setting sections (Hub, Mail, Mon, etc.).
//! Right panel shows editable fields for the selected section.
//! Save writes to `~/.config/cosmix/settings.toml` via cosmix-config.

use dioxus::prelude::*;
use cosmix_ui::theme::{ThemeParams, generate_css};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

// ── Theme params (loaded from config) ──

static THEME: GlobalSignal<ThemeParams> = Signal::global(|| {
    #[cfg(not(target_arch = "wasm32"))]
    {
        cosmix_config::store::load()
            .map(|s| ThemeParams {
                hue: s.global.theme_hue,
                dark: s.global.theme_dark,
                font_size: s.global.font_size,
            })
            .unwrap_or_default()
    }
    #[cfg(target_arch = "wasm32")]
    { ThemeParams::default() }
});

fn main() {
    #[cfg(not(target_arch = "wasm32"))]
    cosmix_ui::desktop::init_linux_env();

    #[cfg(feature = "desktop")]
    {
        let cfg = cosmix_ui::desktop::window_config("Cosmix Settings", 800.0, 600.0);
        LaunchBuilder::new().with_cfg(cfg).launch(app);
        return;
    }

    #[allow(unreachable_code)]
    dioxus::launch(app);
}

// ── Section metadata ──

const SECTIONS: &[(&str, &str)] = &[
    ("global", "Global"),
    ("hub", "Hub"),
    ("web", "Web Server"),
    ("mail", "Mail"),
    ("mon", "Monitor"),
    ("edit", "Editor"),
    ("files", "Files"),
    ("view", "Viewer"),
    ("dns", "DNS"),
    ("wg", "WireGuard"),
    ("backup", "Backup"),
    ("embed", "Embeddings"),
    ("mesh", "Mesh"),
    ("launcher", "Launcher"),
];

// ── Theme presets ──

const PRESETS: &[(&str, f32)] = &[
    ("Ocean", 220.0),
    ("Crimson", 25.0),
    ("Stone", 60.0),
    ("Forest", 150.0),
    ("Sunset", 45.0),
];

// ── App root ──

fn app() -> Element {
    let mut settings = use_signal(|| {
        cosmix_config::store::load().unwrap_or_default()
    });
    let mut active_section = use_signal(|| "global".to_string());
    let mut dirty = use_signal(|| false);
    let mut save_status = use_signal(|| String::new());

    let on_save = move |_| {
        match cosmix_config::store::save(&settings()) {
            Ok(()) => {
                dirty.set(false);
                save_status.set("Saved".into());
                // Update live theme preview
                let s = settings();
                *THEME.write() = ThemeParams {
                    hue: s.global.theme_hue,
                    dark: s.global.theme_dark,
                    font_size: s.global.font_size,
                };
                // Clear status after 2s
                spawn(async move {
                    #[cfg(not(target_arch = "wasm32"))]
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    save_status.set(String::new());
                });
            }
            Err(e) => {
                save_status.set(format!("Error: {e}"));
            }
        }
    };

    let on_reload = move |_| {
        match cosmix_config::store::load() {
            Ok(new) => {
                *THEME.write() = ThemeParams {
                    hue: new.global.theme_hue,
                    dark: new.global.theme_dark,
                    font_size: new.global.font_size,
                };
                settings.set(new);
                dirty.set(false);
                save_status.set("Reloaded".into());
                spawn(async move {
                    #[cfg(not(target_arch = "wasm32"))]
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    save_status.set(String::new());
                });
            }
            Err(e) => {
                save_status.set(format!("Error: {e}"));
            }
        }
    };

    let theme = THEME.read().clone();
    let css = generate_css(&theme);

    rsx! {
        document::Style { "{css}" }
        div {
            style: "width:100%;height:100vh;display:flex;background:var(--bg-primary);color:var(--fg-primary);font-family:var(--font-sans);",

            // Sidebar
            div {
                style: "width:180px;background:var(--bg-secondary);border-right:1px solid var(--border);display:flex;flex-direction:column;padding:8px 0;",

                div {
                    style: "padding:12px 16px;font-size:14px;font-weight:600;color:var(--fg-secondary);",
                    "Settings"
                }

                for (key, label) in SECTIONS.iter() {
                    {
                        let key = key.to_string();
                        let label = *label;
                        let is_active = active_section() == key;
                        let bg = if is_active { "var(--bg-tertiary)" } else { "transparent" };
                        let color = if is_active { "var(--fg-primary)" } else { "var(--fg-muted)" };
                        let border_color = if is_active { "var(--accent)" } else { "transparent" };

                        rsx! {
                            div {
                                style: "padding:8px 16px;cursor:pointer;background:{bg};color:{color};font-size:13px;border-left:3px solid {border_color};",
                                onclick: {
                                    let key = key.clone();
                                    move |_| active_section.set(key.clone())
                                },
                                "{label}"
                            }
                        }
                    }
                }
            }

            // Main panel
            div {
                style: "flex:1;display:flex;flex-direction:column;overflow:hidden;",

                // Content area
                div {
                    style: "flex:1;overflow-y:auto;padding:20px;",

                    section_editor {
                        section: active_section(),
                        settings: settings,
                        dirty: dirty,
                    }
                }

                // Bottom bar
                div {
                    style: "padding:10px 20px;background:var(--bg-secondary);border-top:1px solid var(--border);display:flex;justify-content:space-between;align-items:center;",

                    div {
                        style: "font-size:12px;color:var(--fg-muted);",
                        if !save_status().is_empty() {
                            "{save_status()}"
                        } else if dirty() {
                            "Unsaved changes"
                        } else {
                            ""
                        }
                    }

                    div {
                        style: "display:flex;gap:8px;",

                        button {
                            style: "background:var(--bg-tertiary);border:1px solid var(--border);color:var(--fg-secondary);padding:6px 16px;border-radius:var(--radius-sm);cursor:pointer;font-size:13px;",
                            onclick: on_reload,
                            "Reload"
                        }
                        button {
                            style: "background:var(--accent);border:1px solid var(--accent-hover);color:var(--accent-fg);padding:6px 16px;border-radius:var(--radius-sm);cursor:pointer;font-size:13px;",
                            onclick: on_save,
                            "Save"
                        }
                    }
                }
            }
        }
    }
}

// ── Section editor ──

#[component]
fn section_editor(section: String, settings: Signal<cosmix_config::CosmixSettings>, dirty: Signal<bool>) -> Element {
    let section_data = cosmix_config::store::list_section(&settings(), &section)
        .unwrap_or(serde_json::Value::Object(Default::default()));

    let fields: Vec<(String, serde_json::Value)> = match section_data {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<_> = map.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            entries
        }
        _ => Vec::new(),
    };

    let section_label = SECTIONS.iter()
        .find(|(k, _)| *k == section.as_str())
        .map(|(_, l)| *l)
        .unwrap_or_else(|| section.as_str());

    rsx! {
        h2 {
            style: "margin:0 0 16px 0;font-size:18px;font-weight:600;",
            "{section_label}"
        }

        // Theme presets for Global section
        if section == "global" {
            theme_presets { settings: settings, dirty: dirty }
        }

        for (key, value) in fields.iter() {
            {
                let dotpath = format!("{section}.{key}");
                let display_key = key.replace('_', " ");

                rsx! {
                    div {
                        style: "margin-bottom:12px;display:flex;align-items:center;gap:12px;",

                        label {
                            style: "width:180px;font-size:13px;color:var(--fg-secondary);text-transform:capitalize;flex-shrink:0;",
                            "{display_key}"
                        }

                        {field_input(dotpath, value.clone(), settings, dirty)}
                    }
                }
            }
        }
    }
}

// ── Theme preset swatches ──

#[component]
fn theme_presets(settings: Signal<cosmix_config::CosmixSettings>, dirty: Signal<bool>) -> Element {
    rsx! {
        div {
            style: "margin-bottom:20px;padding:12px;background:var(--bg-secondary);border-radius:var(--radius-md);",

            div { style: "font-size:13px;font-weight:600;color:var(--fg-secondary);margin-bottom:10px;", "Theme Presets" }

            div {
                style: "display:flex;gap:8px;flex-wrap:wrap;",
                for (name, hue) in PRESETS.iter() {
                    {
                        let hue = *hue;
                        let name = *name;
                        let current_hue = settings().global.theme_hue;
                        let is_active = (current_hue - hue).abs() < 1.0;
                        let border = if is_active { "2px solid var(--fg-primary)" } else { "2px solid var(--border)" };
                        // Show a swatch with the preset's accent colour
                        let swatch_color = format!("oklch(75% 0.12 {hue:.1})");

                        rsx! {
                            button {
                                style: "display:flex;flex-direction:column;align-items:center;gap:4px;padding:8px 12px;background:var(--bg-tertiary);border:{border};border-radius:var(--radius-md);cursor:pointer;",
                                onclick: move |_| {
                                    let ok = cosmix_config::store::set_value(
                                        &mut settings.write(),
                                        "global.theme_hue",
                                        serde_json::json!(hue),
                                    ).is_ok();
                                    if ok {
                                        dirty.set(true);
                                        let s = settings();
                                        *THEME.write() = ThemeParams {
                                            hue: s.global.theme_hue,
                                            dark: s.global.theme_dark,
                                            font_size: s.global.font_size,
                                        };
                                    }
                                },
                                div { style: "width:24px;height:24px;border-radius:50%;background:{swatch_color};" }
                                span { style: "font-size:11px;color:var(--fg-secondary);", "{name}" }
                            }
                        }
                    }
                }
            }

            // Dark/Light toggle
            div {
                style: "margin-top:12px;display:flex;align-items:center;gap:8px;",
                span { style: "font-size:13px;color:var(--fg-secondary);", "Dark mode" }
                input {
                    r#type: "checkbox",
                    checked: settings().global.theme_dark,
                    style: "width:18px;height:18px;accent-color:var(--accent);",
                    onchange: move |e: Event<FormData>| {
                        let is_dark = e.value() == "true";
                        let ok = cosmix_config::store::set_value(
                            &mut settings.write(),
                            "global.theme_dark",
                            serde_json::json!(is_dark),
                        ).is_ok();
                        if ok {
                            dirty.set(true);
                            let s = settings();
                            *THEME.write() = ThemeParams {
                                hue: s.global.theme_hue,
                                dark: s.global.theme_dark,
                                font_size: s.global.font_size,
                            };
                        }
                    },
                }
            }
        }
    }
}

fn field_input(
    dotpath: String,
    value: serde_json::Value,
    mut settings: Signal<cosmix_config::CosmixSettings>,
    mut dirty: Signal<bool>,
) -> Element {
    match &value {
        serde_json::Value::Bool(b) => {
            let checked = *b;
            rsx! {
                input {
                    r#type: "checkbox",
                    checked: checked,
                    style: "width:18px;height:18px;accent-color:var(--accent);",
                    onchange: move |e: Event<FormData>| {
                        let new_val = serde_json::Value::Bool(e.value() == "true");
                        if let Ok(()) = cosmix_config::store::set_value(&mut settings.write(), &dotpath, new_val) {
                            dirty.set(true);
                        }
                    },
                }
            }
        }
        serde_json::Value::Number(n) => {
            let display = n.to_string();
            // Use a range slider for theme_hue
            let is_hue = dotpath == "global.theme_hue";
            if is_hue {
                rsx! {
                    div { style: "display:flex;align-items:center;gap:8px;flex:1;",
                        input {
                            r#type: "range",
                            min: "0",
                            max: "360",
                            step: "1",
                            value: "{display}",
                            style: "flex:1;accent-color:var(--accent);",
                            oninput: {
                                let dotpath = dotpath.clone();
                                move |e: Event<FormData>| {
                                    if let Ok(f) = e.value().parse::<f64>() {
                                        let new_val = serde_json::json!(f);
                                        let ok = cosmix_config::store::set_value(&mut settings.write(), &dotpath, new_val).is_ok();
                                        if ok {
                                            dirty.set(true);
                                            let s = settings();
                                            *THEME.write() = ThemeParams {
                                                hue: s.global.theme_hue,
                                                dark: s.global.theme_dark,
                                                font_size: s.global.font_size,
                                            };
                                        }
                                    }
                                }
                            },
                        }
                        span { style: "font-size:12px;color:var(--fg-muted);min-width:30px;", "{display}" }
                    }
                }
            } else {
                rsx! {
                    input {
                        r#type: "number",
                        value: "{display}",
                        style: "flex:1;background:var(--bg-tertiary);border:1px solid var(--border);color:var(--fg-primary);padding:6px 10px;border-radius:var(--radius-sm);font-size:13px;outline:none;font-family:var(--font-mono);max-width:120px;",
                        onchange: {
                            let dotpath = dotpath.clone();
                            move |e: Event<FormData>| {
                                let text = e.value();
                                let new_val = if let Ok(i) = text.parse::<i64>() {
                                    serde_json::json!(i)
                                } else if let Ok(f) = text.parse::<f64>() {
                                    serde_json::json!(f)
                                } else {
                                    return;
                                };
                                if let Ok(()) = cosmix_config::store::set_value(&mut settings.write(), &dotpath, new_val) {
                                    dirty.set(true);
                                }
                            }
                        },
                    }
                }
            }
        }
        _ => {
            // String and everything else — text input
            let display = match &value {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let is_password = dotpath.contains("password");
            rsx! {
                input {
                    r#type: if is_password { "password" } else { "text" },
                    value: "{display}",
                    style: "flex:1;background:var(--bg-tertiary);border:1px solid var(--border);color:var(--fg-primary);padding:6px 10px;border-radius:var(--radius-sm);font-size:13px;outline:none;font-family:var(--font-mono);",
                    onchange: {
                        let dotpath = dotpath.clone();
                        move |e: Event<FormData>| {
                            let new_val = serde_json::Value::String(e.value());
                            if let Ok(()) = cosmix_config::store::set_value(&mut settings.write(), &dotpath, new_val) {
                                dirty.set(true);
                            }
                        }
                    },
                }
            }
        }
    }
}
