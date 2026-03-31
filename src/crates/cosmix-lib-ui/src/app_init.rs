//! Shared app initialization for all cosmix Dioxus apps.
//!
//! Provides the global THEME signal, theme polling/hub-watch hooks,
//! and the `launch_desktop()` helper that replaces boilerplate in every app's `main()`.

use dioxus::prelude::*;
use crate::theme::{ThemeParams, generate_css};

// ── Global theme signal ──

/// Global theme signal loaded from cosmix config on startup.
/// All apps should use this instead of declaring their own static THEME.
pub static THEME: GlobalSignal<ThemeParams> = Signal::global(|| {
    #[cfg(all(not(target_arch = "wasm32"), feature = "config"))]
    {
        cosmix_config::store::load()
            .map(|s| ThemeParams {
                hue: s.global.theme_hue,
                dark: s.global.theme_dark,
                font_size: s.global.font_size,
            })
            .unwrap_or_default()
    }
    #[cfg(any(target_arch = "wasm32", not(feature = "config")))]
    {
        ThemeParams::default()
    }
});

/// Reload THEME from config file. Call from any context that can write signals.
#[cfg(all(not(target_arch = "wasm32"), feature = "config"))]
pub fn reload_theme() {
    if let Ok(settings) = cosmix_config::store::load() {
        *THEME.write() = ThemeParams {
            hue: settings.global.theme_hue,
            dark: settings.global.theme_dark,
            font_size: settings.global.font_size,
        };
    }
}

// ── Theme CSS injection ──

/// Injects theme CSS into the document and reactively updates it when THEME changes.
///
/// Uses `document::eval()` to create/update a `<style id="cosmix-theme">` element,
/// because `document::Style` is write-once and ignores prop changes after first render.
///
/// Call once in your app's root component. No `document::Style` needed in rsx.
///
/// ```ignore
/// fn app() -> Element {
///     use_theme_css();
///     rsx! { div { style: "background:var(--bg-primary);", "themed!" } }
/// }
/// ```
pub fn use_theme_css() {
    use_effect(move || {
        let theme = THEME.read();
        let css = generate_css(&theme);
        // Escape backticks in CSS (unlikely but safe)
        let css = css.replace('`', "\\`");
        document::eval(&format!(
            r#"
            let el = document.getElementById('cosmix-theme');
            if (!el) {{
                el = document.createElement('style');
                el.id = 'cosmix-theme';
                document.head.appendChild(el);
            }}
            el.textContent = `{css}`;
            "#
        ));
    });
}

// ── Theme polling hook ──

/// Spawns a background task that reloads the theme from config every `interval_secs`.
/// Call once in your app's root component. Requires `config` feature.
///
/// For apps connected to the hub, prefer `use_theme_hub_watch()` instead.
#[cfg(all(not(target_arch = "wasm32"), feature = "config"))]
pub fn use_theme_poll(interval_secs: u64) {
    use_effect(move || {
        spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(interval_secs)).await;
                reload_theme();
            }
        });
    });
}

// ── Theme hub watch hook ──

/// Registers with configd for `config.changed` notifications and updates THEME.
/// Also handles the `config.changed` command in the incoming command stream.
///
/// This is the preferred theme refresh method for hub-connected apps.
/// Requires both `hub` and `config` features.
#[cfg(all(not(target_arch = "wasm32"), feature = "hub", feature = "config"))]
pub fn use_theme_hub_watch(
    client: Signal<Option<std::sync::Arc<cosmix_client::HubClient>>>,
    service_name: &'static str,
) {
    use_effect(move || {
        spawn(async move {
            // Wait for hub client to connect
            loop {
                if client().is_some() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }

            if let Some(c) = client() {
                // Register as config watcher
                let _ = c.call(
                    "config",
                    "config.watch",
                    serde_json::json!({ "watcher": service_name }),
                ).await;
            }
        });
    });
}

/// Handle a `config.changed` command by reloading the theme.
/// Call this from your hub command handler's match arms:
///
/// ```ignore
/// "config.changed" => { cosmix_ui::app_init::handle_config_changed(); Ok("ok".into()) }
/// ```
#[cfg(all(not(target_arch = "wasm32"), feature = "config"))]
pub fn handle_config_changed() {
    reload_theme();
}

// ── Hub connection hooks ──

/// Connect to the hub as a named service. Returns a signal that is set
/// once the connection succeeds (or stays `None` if the hub is unavailable).
///
/// Call once in your app's root component. Requires `hub` feature.
///
/// ```ignore
/// let hub = use_hub_client("files");
/// ```
#[cfg(all(not(target_arch = "wasm32"), feature = "hub"))]
pub fn use_hub_client(
    service_name: &'static str,
) -> Signal<Option<std::sync::Arc<cosmix_client::HubClient>>> {
    let mut client_sig: Signal<Option<std::sync::Arc<cosmix_client::HubClient>>> =
        use_signal(|| None);

    use_effect(move || {
        spawn(async move {
            match cosmix_client::HubClient::connect_default(service_name).await {
                Ok(c) => {
                    let client = std::sync::Arc::new(c);
                    tracing::info!("connected to cosmix-hub as '{service_name}'");
                    client_sig.set(Some(client));
                }
                Err(_) => {
                    tracing::debug!("hub not available, running standalone");
                }
            }
        });
    });

    client_sig
}

/// Spawn a command handler loop for the hub client.
///
/// Automatically registers with configd for `config.changed` notifications
/// (when `config` feature is enabled) and handles them by reloading the theme.
///
/// The `handler` receives all other commands and should return `Ok(body)` or
/// `Err(message)`. Response sending is handled automatically with RC 0 for
/// success and RC 10 for errors.
///
/// For apps with async command handlers, keep your own loop and call
/// `handle_config_changed()` for the `"config.changed"` command.
///
/// ```ignore
/// let hub = use_hub_client("files");
/// use_hub_handler(hub, "files", |cmd| match cmd.command.as_str() {
///     "file.list" => Ok(r#"[]"#.to_string()),
///     _ => Err(format!("unknown: {}", cmd.command)),
/// });
/// ```
#[cfg(all(not(target_arch = "wasm32"), feature = "hub"))]
pub fn use_hub_handler<F>(
    client: Signal<Option<std::sync::Arc<cosmix_client::HubClient>>>,
    service_name: &'static str,
    handler: F,
) where
    F: Fn(&cosmix_client::IncomingCommand) -> Result<String, String> + Send + Sync + 'static + Clone,
{
    use_effect(move || {
        let handler = handler.clone();
        spawn(async move {
            // Wait for client to connect
            loop {
                if client().is_some() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }

            let Some(c) = client() else { return };

            // Register for config change notifications (fire-and-forget —
            // must not block if configd is not running, otherwise the command
            // dispatch loop below never starts).
            #[cfg(feature = "config")]
            {
                let _ = c
                    .send(
                        "config",
                        "config.watch",
                        serde_json::json!({ "watcher": service_name }),
                    )
                    .await;
            }

            // Command dispatch loop
            let Some(mut rx) = c.incoming_async().await else {
                return;
            };

            while let Some(cmd) = rx.recv().await {
                eprintln!("[hub-handler:{service_name}] cmd={} from={}", cmd.command, cmd.from);
                let result = match cmd.command.as_str() {
                    #[cfg(feature = "config")]
                    "config.changed" => {
                        reload_theme();
                        Ok(r#"{"status":"ok"}"#.to_string())
                    }
                    "menu.list" => handle_menu_list(&cmd),
                    "menu.invoke" | "menu.highlight" | "menu.close" => handle_menu_command(&cmd),
                    "ui.list" => handle_ui_list(&cmd),
                    "ui.get" => handle_ui_get(&cmd),
                    "ui.invoke" | "ui.highlight" | "ui.set" => handle_ui_command(&cmd),
                    "ui.batch" => handle_ui_batch(&cmd),
                    _ => handler(&cmd),
                };

                match &result {
                    Ok(body) => eprintln!("[hub-handler:{service_name}] ok body={}B", body.len()),
                    Err(msg) => eprintln!("[hub-handler:{service_name}] err={msg}"),
                }

                match result {
                    Ok(body) => {
                        if let Err(e) = c.respond(&cmd, 0, &body).await {
                            eprintln!("[hub-handler:{service_name}] respond FAILED: {e}");
                        }
                    }
                    Err(msg) => {
                        let err_body = serde_json::json!({"error": msg}).to_string();
                        if let Err(e) = c.respond(&cmd, 10, &err_body).await {
                            eprintln!("[hub-handler:{service_name}] respond err FAILED: {e}");
                        }
                    }
                }
            }
        });
    });
}

// ── Desktop launch helper ──

/// Standard desktop app launcher. Replaces the boilerplate in every app's `main()`.
///
/// Handles:
/// - `init_linux_env()` (WebKit workaround)
/// - Window config (frameless, CSD)
/// - `LaunchBuilder` with desktop config
/// - WASM fallback with `dioxus::launch()`
///
/// Usage:
/// ```ignore
/// fn main() {
///     cosmix_ui::app_init::launch_desktop("Cosmix Files", 900.0, 640.0, app);
/// }
/// ```
pub fn launch_desktop(title: &str, width: f64, height: f64, app_fn: fn() -> Element) {
    #[cfg(not(target_arch = "wasm32"))]
    let _log_guard = init_app_tracing(title);

    #[cfg(all(not(target_arch = "wasm32"), feature = "desktop"))]
    {
        crate::desktop::init_linux_env();
        let cfg = crate::desktop::window_config(title, width, height);
        LaunchBuilder::new().with_cfg(cfg).launch(app_fn);
        return;
    }

    #[allow(unreachable_code)]
    {
        dioxus::launch(app_fn);
    }
}

/// Initialize tracing for a GUI app: stderr + daily log file.
///
/// Log files: `~/.local/log/cosmix/{app_name}.YYYY-MM-DD.log`
#[cfg(not(target_arch = "wasm32"))]
pub fn init_app_tracing(app_name: &str) -> tracing_appender::non_blocking::WorkerGuard {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    // Derive a log-friendly name: "Cosmix Files" → "cosmix-files"
    let log_name = app_name
        .to_lowercase()
        .replace(' ', "-");

    let default = format!("{}=info", log_name.replace('-', "_"));
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| default.into());

    let log_dir = std::env::var("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".local/log/cosmix"))
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp/cosmix-log"));
    let _ = std::fs::create_dir_all(&log_dir);

    let file_appender = tracing_appender::rolling::daily(&log_dir, &log_name);
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(non_blocking)
                .with_ansi(false),
        )
        .init();

    guard
}

// ── Menu AMP command handlers ──

/// Handle `menu.list` — return all menu items as JSON.
#[cfg(all(not(target_arch = "wasm32"), feature = "hub"))]
fn handle_menu_list(
    _cmd: &cosmix_client::IncomingCommand,
) -> Result<String, String> {
    use crate::menu::MENU_DEF;

    let def = MENU_DEF.read();
    match def.as_ref() {
        Some(menu) => {
            let items: Vec<serde_json::Value> = menu
                .collect_items()
                .iter()
                .map(|i| i.to_json_value())
                .collect();
            Ok(serde_json::to_string(&items).unwrap_or_else(|_| "[]".to_string()))
        }
        None => Ok("[]".to_string()),
    }
}

/// Handle `menu.invoke`, `menu.highlight`, `menu.close` — write to MENU_CMD signal.
#[cfg(all(not(target_arch = "wasm32"), feature = "hub"))]
fn handle_menu_command(
    cmd: &cosmix_client::IncomingCommand,
) -> Result<String, String> {
    use crate::menu::{MenuCommand, MENU_CMD};

    let menu_cmd = match cmd.command.as_str() {
        "menu.close" => MenuCommand::Close,
        "menu.invoke" => {
            let id = cmd.args.get("id")
                .and_then(|v| v.as_str())
                .ok_or("menu.invoke requires {\"id\": \"...\"}")?;
            MenuCommand::Invoke { id: id.to_string() }
        }
        "menu.highlight" => {
            let id = cmd.args.get("id")
                .and_then(|v| v.as_str())
                .ok_or("menu.highlight requires {\"id\": \"...\"}")?;
            let ms = cmd.args.get("ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(500) as u32;
            MenuCommand::Highlight { id: id.to_string(), duration_ms: ms }
        }
        _ => return Err(format!("unknown menu command: {}", cmd.command)),
    };

    *MENU_CMD.write() = Some(menu_cmd);
    Ok(r#"{"status":"ok"}"#.to_string())
}

// ── UI element AMP command handlers ──

/// Handle `ui.list` — return all registered UI elements as JSON.
#[cfg(all(not(target_arch = "wasm32"), feature = "hub"))]
fn handle_ui_list(
    cmd: &cosmix_client::IncomingCommand,
) -> Result<String, String> {
    use crate::components::UI_REGISTRY;

    let prefix = cmd.args.get("prefix").and_then(|v| v.as_str());
    let registry = UI_REGISTRY.read();
    let items = registry.list(prefix);
    Ok(serde_json::to_string(&items).unwrap_or_else(|_| "[]".to_string()))
}

/// Handle `ui.get` — return current state of specific elements.
///
/// Accepts `{"id": "..."}` for a single element or `{"ids": ["a","b"]}` for multiple.
#[cfg(all(not(target_arch = "wasm32"), feature = "hub"))]
fn handle_ui_get(
    cmd: &cosmix_client::IncomingCommand,
) -> Result<String, String> {
    use crate::components::UI_REGISTRY;

    let registry = UI_REGISTRY.read();

    // Support single id or array of ids
    let ids: Vec<&str> = if let Some(id) = cmd.args.get("id").and_then(|v| v.as_str()) {
        vec![id]
    } else if let Some(arr) = cmd.args.get("ids").and_then(|v| v.as_array()) {
        arr.iter().filter_map(|v| v.as_str()).collect()
    } else {
        return Err("ui.get requires {\"id\": \"...\"} or {\"ids\": [...]}".to_string());
    };

    let items = registry.get(&ids);
    Ok(serde_json::to_string(&items).unwrap_or_else(|_| "[]".to_string()))
}

/// Handle `ui.invoke`, `ui.highlight`, `ui.set` — write to UI_CMD signal.
#[cfg(all(not(target_arch = "wasm32"), feature = "hub"))]
fn handle_ui_command(
    cmd: &cosmix_client::IncomingCommand,
) -> Result<String, String> {
    use crate::components::{UiCommand, UI_CMD};

    let id = cmd.args.get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("{} requires {{\"id\": \"...\"}}", cmd.command))?
        .to_string();

    let ui_cmd = match cmd.command.as_str() {
        "ui.invoke" => UiCommand::Invoke,
        "ui.highlight" => {
            let ms = cmd.args.get("ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(500) as u32;
            UiCommand::Highlight { duration_ms: ms }
        }
        "ui.set" => {
            let value = cmd.args.get("value")
                .and_then(|v| v.as_str())
                .ok_or("ui.set requires {\"id\": \"...\", \"value\": \"...\"}")?
                .to_string();
            UiCommand::SetValue(value)
        }
        _ => return Err(format!("unknown ui command: {}", cmd.command)),
    };

    *UI_CMD.write() = Some((id, ui_cmd));
    Ok(r#"{"status":"ok"}"#.to_string())
}

/// Handle `ui.batch` — process multiple UI actions in a single round trip.
///
/// Body is a JSON array of action objects:
/// ```json
/// [
///   {"command": "menu.invoke", "id": "save"},
///   {"command": "ui.invoke", "id": "file.open"},
///   {"command": "ui.highlight", "id": "file.save", "ms": 500},
///   {"command": "menu.close"}
/// ]
/// ```
///
/// Returns results array with rc per action.
#[cfg(all(not(target_arch = "wasm32"), feature = "hub"))]
fn handle_ui_batch(
    cmd: &cosmix_client::IncomingCommand,
) -> Result<String, String> {
    use crate::menu::{MenuCommand, MENU_CMD};
    use crate::components::{UiCommand, UI_CMD};

    // args can be the array directly, or {"actions": [...]}
    let actions = cmd.args.as_array()
        .or_else(|| cmd.args.get("actions").and_then(|v| v.as_array()))
        .ok_or("ui.batch requires a JSON array of actions (or {\"actions\": [...]})")?;

    let mut results = Vec::new();

    for action in actions {
        let command = action.get("command").and_then(|v| v.as_str()).unwrap_or("");
        let id = action.get("id").and_then(|v| v.as_str());

        let result = match command {
            "menu.close" => {
                *MENU_CMD.write() = Some(MenuCommand::Close);
                serde_json::json!({"command": command, "rc": 0})
            }
            "menu.invoke" => {
                if let Some(id) = id {
                    *MENU_CMD.write() = Some(MenuCommand::Invoke { id: id.to_string() });
                    serde_json::json!({"command": command, "id": id, "rc": 0})
                } else {
                    serde_json::json!({"command": command, "rc": 10, "error": "missing id"})
                }
            }
            "menu.highlight" => {
                if let Some(id) = id {
                    let ms = action.get("ms").and_then(|v| v.as_u64()).unwrap_or(500) as u32;
                    *MENU_CMD.write() = Some(MenuCommand::Highlight { id: id.to_string(), duration_ms: ms });
                    serde_json::json!({"command": command, "id": id, "rc": 0})
                } else {
                    serde_json::json!({"command": command, "rc": 10, "error": "missing id"})
                }
            }
            "ui.invoke" => {
                if let Some(id) = id {
                    *UI_CMD.write() = Some((id.to_string(), UiCommand::Invoke));
                    serde_json::json!({"command": command, "id": id, "rc": 0})
                } else {
                    serde_json::json!({"command": command, "rc": 10, "error": "missing id"})
                }
            }
            "ui.highlight" => {
                if let Some(id) = id {
                    let ms = action.get("ms").and_then(|v| v.as_u64()).unwrap_or(500) as u32;
                    *UI_CMD.write() = Some((id.to_string(), UiCommand::Highlight { duration_ms: ms }));
                    serde_json::json!({"command": command, "id": id, "rc": 0})
                } else {
                    serde_json::json!({"command": command, "rc": 10, "error": "missing id"})
                }
            }
            "ui.set" => {
                match (id, action.get("value").and_then(|v| v.as_str())) {
                    (Some(id), Some(value)) => {
                        *UI_CMD.write() = Some((id.to_string(), UiCommand::SetValue(value.to_string())));
                        serde_json::json!({"command": command, "id": id, "rc": 0})
                    }
                    _ => serde_json::json!({"command": command, "rc": 10, "error": "missing id or value"}),
                }
            }
            _ => serde_json::json!({"command": command, "rc": 10, "error": "unknown batch command"}),
        };
        results.push(result);
    }

    Ok(serde_json::to_string(&results).unwrap_or_else(|_| "[]".to_string()))
}
