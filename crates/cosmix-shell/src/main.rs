//! cosmix-shell — DCS shell binary entry point.

use dioxus::prelude::*;

#[cfg(not(target_arch = "wasm32"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    #[cfg(not(target_arch = "wasm32"))]
    cosmix_ui::desktop::init_linux_env();

    #[cfg(not(target_arch = "wasm32"))]
    {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "cosmix_shell=info".into()),
            )
            .init();
    }

    #[cfg(feature = "desktop")]
    {
        let cfg = cosmix_ui::desktop::window_config("cosmix-shell", 1400.0, 900.0);
        LaunchBuilder::new().with_cfg(cfg).launch(app);
        return;
    }

    #[allow(unreachable_code)]
    dioxus::launch(app);
}

fn app() -> Element {
    use std::sync::Arc;

    let mut hub_client: Signal<Option<Arc<cosmix_client::HubClient>>> = use_signal(|| None);

    // Connect to hub
    use_effect(move || {
        spawn(async move {
            #[cfg(not(target_arch = "wasm32"))]
            let client = cosmix_client::HubClient::connect_default("shell").await;
            #[cfg(target_arch = "wasm32")]
            let client = cosmix_client::HubClient::connect_anonymous_default();

            match client {
                Ok(c) => {
                    let client = Arc::new(c);
                    tracing::info!("connected to cosmix-hub as 'shell'");
                    hub_client.set(Some(client.clone()));

                    // Register for config changes
                    let _ = client.call(
                        "configd",
                        "config.watch",
                        serde_json::json!({ "watcher": "shell" }),
                    ).await;

                    // Handle incoming commands
                    #[cfg(not(target_arch = "wasm32"))]
                    {
                        let client2 = client.clone();
                        tokio::spawn(async move {
                            if let Some(mut rx) = client2.incoming_async().await {
                                while let Some(cmd) = rx.recv().await {
                                    if cmd.command == "config.changed" {
                                        if let Ok(settings) = cosmix_config::store::load() {
                                            *cosmix_shell::THEME.write() = cosmix_ui::theme::ThemeParams {
                                                hue: settings.global.theme_hue,
                                                dark: settings.global.theme_dark,
                                                font_size: settings.global.font_size,
                                            };
                                        }
                                        let _ = client2.respond(&cmd, 0, r#"{"status":"ok"}"#).await;
                                    }
                                }
                            }
                        });
                    }
                }
                Err(e) => {
                    tracing::debug!("hub not available: {e}");
                }
            }
        });
    });

    // Poll config every 30s as fallback (desktop only)
    #[cfg(not(target_arch = "wasm32"))]
    use_effect(move || {
        spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                if let Ok(settings) = cosmix_config::store::load() {
                    let new = cosmix_ui::theme::ThemeParams {
                        hue: settings.global.theme_hue,
                        dark: settings.global.theme_dark,
                        font_size: settings.global.font_size,
                    };
                    let current = cosmix_shell::THEME.read();
                    if new.font_size != current.font_size
                        || new.hue != current.hue
                        || new.dark != current.dark
                    {
                        drop(current);
                        *cosmix_shell::THEME.write() = new;
                    }
                }
            }
        });
    });

    cosmix_shell::shell_app()
}
