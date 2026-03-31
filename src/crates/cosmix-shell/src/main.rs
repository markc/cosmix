//! cosmix-shell — DCS shell binary entry point.

use dioxus::prelude::*;

#[cfg(not(target_arch = "wasm32"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    cosmix_ui::app_init::launch_desktop("cosmix-shell", 1400.0, 900.0, app);
}

fn app() -> Element {
    #[cfg(not(target_arch = "wasm32"))]
    let _hub_client = {
        use cosmix_ui::app_init::{use_hub_client, use_hub_handler};
        let hub_client = use_hub_client("shell");
        use_hub_handler(hub_client, "shell", |cmd| {
            Err(format!("unknown command: {}", cmd.command))
        });
        hub_client
    };

    // Poll config every 30s as fallback (desktop only)
    #[cfg(not(target_arch = "wasm32"))]
    cosmix_ui::app_init::use_theme_poll(30);

    cosmix_shell::shell_app()
}
