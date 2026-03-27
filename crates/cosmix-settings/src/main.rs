//! cosmix-settings — GUI settings editor for the cosmix stack.
//!
//! Sidebar lists setting sections (Hub, Mail, Mon, etc.).
//! Right panel shows editable fields for the selected section.
//! Save writes to `~/.config/cosmix/settings.toml` via cosmix-config.

use dioxus::prelude::*;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    #[cfg(not(target_arch = "wasm32"))]
    cosmix_ui::desktop::init_linux_env();

    #[cfg(feature = "desktop")]
    {
        use dioxus_desktop::{muda::Menu, Config, LogicalSize, WindowBuilder};

        let cfg = Config::new()
            .with_window(
                WindowBuilder::new()
                    .with_title("Cosmix Settings")
                    .with_inner_size(LogicalSize::new(800.0, 600.0)),
            )
            .with_menu(Menu::new());

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

    rsx! {
        document::Style { "{CSS}" }
        div {
            style: "width:100%;height:100vh;display:flex;background:{BG_BASE};color:{TEXT_PRIMARY};font-family:sans-serif;",

            // Sidebar
            div {
                style: "width:180px;background:{BG_SURFACE};border-right:1px solid {BORDER};display:flex;flex-direction:column;padding:8px 0;",

                div {
                    style: "padding:12px 16px;font-size:14px;font-weight:600;color:{TEXT_SECONDARY};",
                    "Settings"
                }

                for (key, label) in SECTIONS.iter() {
                    {
                        let key = key.to_string();
                        let label = *label;
                        let is_active = active_section() == key;
                        let bg = if is_active { BG_ELEVATED } else { "transparent" };
                        let color = if is_active { TEXT_PRIMARY } else { TEXT_MUTED };
                        let border_color = if is_active { ACCENT_BLUE } else { "transparent" };

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
                    style: "padding:10px 20px;background:{BG_SURFACE};border-top:1px solid {BORDER};display:flex;justify-content:space-between;align-items:center;",

                    div {
                        style: "font-size:12px;color:{TEXT_MUTED};",
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
                            style: "{BTN_STYLE}",
                            onclick: on_reload,
                            "Reload"
                        }
                        button {
                            style: "{BTN_PRIMARY_STYLE}",
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

        for (key, value) in fields.iter() {
            {
                let dotpath = format!("{section}.{key}");
                let display_key = key.replace('_', " ");

                rsx! {
                    div {
                        style: "margin-bottom:12px;display:flex;align-items:center;gap:12px;",

                        label {
                            style: "width:180px;font-size:13px;color:{TEXT_SECONDARY};text-transform:capitalize;flex-shrink:0;",
                            "{display_key}"
                        }

                        {field_input(dotpath, value.clone(), settings, dirty)}
                    }
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
                    style: "width:18px;height:18px;accent-color:{ACCENT_BLUE};",
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
            rsx! {
                input {
                    r#type: "number",
                    value: "{display}",
                    style: "{INPUT_STYLE}",
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
                    style: "{INPUT_STYLE}",
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

// ── Theme ──

const BG_BASE: &str = cosmix_ui::theme::BG_BASE;
const BG_SURFACE: &str = cosmix_ui::theme::BG_SURFACE;
const BG_ELEVATED: &str = cosmix_ui::theme::BG_ELEVATED;
const BORDER: &str = cosmix_ui::theme::BORDER_DEFAULT;
const TEXT_PRIMARY: &str = cosmix_ui::theme::TEXT_PRIMARY;
const TEXT_SECONDARY: &str = cosmix_ui::theme::TEXT_SECONDARY;
const TEXT_MUTED: &str = cosmix_ui::theme::TEXT_MUTED;
const ACCENT_BLUE: &str = cosmix_ui::theme::ACCENT_BLUE;

const BTN_STYLE: &str = "background:#374151;border:1px solid #4b5563;color:#d1d5db;padding:6px 16px;border-radius:4px;cursor:pointer;font-size:13px;";
const BTN_PRIMARY_STYLE: &str = "background:#2563eb;border:1px solid #3b82f6;color:#fff;padding:6px 16px;border-radius:4px;cursor:pointer;font-size:13px;";
const INPUT_STYLE: &str = "flex:1;background:#1f2937;border:1px solid #374151;color:#f3f4f6;padding:6px 10px;border-radius:4px;font-size:13px;outline:none;font-family:monospace;";

const CSS: &str = r#"
html, body, #main {
    margin: 0; padding: 0;
    width: 100%; height: 100%;
    overflow: hidden;
}
button:hover { filter: brightness(1.2); }
input:focus { border-color: #3b82f6 !important; }
input[type="number"] { max-width: 120px; }
::-webkit-scrollbar { width: 8px; }
::-webkit-scrollbar-track { background: transparent; }
::-webkit-scrollbar-thumb { background: #374151; border-radius: 4px; }
::-webkit-scrollbar-thumb:hover { background: #4b5563; }
"#;
