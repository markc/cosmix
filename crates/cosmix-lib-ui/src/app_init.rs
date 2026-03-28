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

// ── Theme CSS hook ──

/// Returns a reactive memo of the current theme CSS string.
///
/// The memo automatically re-computes when THEME changes, ensuring
/// `document::Style` updates immediately.
///
/// Usage in any app's `app()`:
/// ```ignore
/// let css = use_theme_css();
/// rsx! { document::Style { "{css}" } ... }
/// ```
pub fn use_theme_css() -> Memo<String> {
    use_memo(move || {
        let theme = THEME.read();
        generate_css(&theme)
    })
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
                    "configd",
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

            // Register for config change notifications
            #[cfg(feature = "config")]
            {
                let _ = c
                    .call(
                        "configd",
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
                let result = match cmd.command.as_str() {
                    #[cfg(feature = "config")]
                    "config.changed" => {
                        reload_theme();
                        Ok(r#"{"status":"ok"}"#.to_string())
                    }
                    _ => handler(&cmd),
                };

                match result {
                    Ok(body) => {
                        if let Err(e) = c.respond(&cmd, 0, &body).await {
                            tracing::warn!("failed to send response: {e}");
                        }
                    }
                    Err(msg) => {
                        let err_body = serde_json::json!({"error": msg}).to_string();
                        if let Err(e) = c.respond(&cmd, 10, &err_body).await {
                            tracing::warn!("failed to send error response: {e}");
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
