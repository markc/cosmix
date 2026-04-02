//! cosmix-dialog — GUI dialog utility for the Cosmix stack.
//!
//! Usage:
//!   cosmix-dialog info --text="Hello world"
//!   cosmix-dialog confirm --text="Continue?"
//!   cosmix-dialog input --text="Name:" --entry-text="Mark"
//!   cosmix-dialog password --text="Secret:"
//!   echo "log text" | cosmix-dialog text-info

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod components;

use std::sync::OnceLock;

use clap::Parser;
use dioxus::prelude::*;

use cosmix_dialog::cli::Cli;
use cosmix_dialog::render::DialogView;
use cosmix_dialog::window::{self, RESULT};
use cosmix_dialog::DialogRequest;
use cosmix_ui::app_init::use_theme_css;
use cosmix_ui::menu::{MenuBar, MenuBarDef};

/// The parsed dialog request, set once before Dioxus launches.
static REQUEST: OnceLock<DialogRequest> = OnceLock::new();

fn main() {
    let cli = Cli::parse();

    // Read stdin if this is a text-info dialog
    let stdin_text = if matches!(cli.mode, cosmix_dialog::cli::CliMode::TextInfo) {
        use std::io::Read;
        let mut buf = String::new();
        let _ = std::io::stdin().read_to_string(&mut buf);
        Some(buf)
    } else {
        None
    };

    let request = cli.into_request(stdin_text);
    let title = request.effective_title().to_string();
    let (w, h) = request.default_size();
    let json_output = request.json_output;

    REQUEST.set(request).expect("REQUEST already set");
    window::JSON_OUTPUT.store(json_output, std::sync::atomic::Ordering::SeqCst);

    #[cfg(feature = "desktop")]
    {
        cosmix_ui::desktop::init_linux_env();
        let cfg = window::dialog_window_config(&title, w as f64, h as f64);
        LaunchBuilder::new().with_cfg(cfg).launch(app);
    }

    #[cfg(not(feature = "desktop"))]
    {
        dioxus::launch(app);
    }

    // After window closes, output result to stdout
    if let Some(result) = RESULT.peek().as_ref() {
        let output = result.to_stdout(json_output);
        if !output.is_empty() {
            print!("{output}");
        }
    }
}

fn app() -> Element {
    use_theme_css();

    let request = REQUEST.get().expect("REQUEST not set").clone();

    // Empty menu — dialog only needs the caption buttons (drag + close)
    let menu = MenuBarDef::new();

    rsx! {
        document::Stylesheet { href: asset!("/assets/tailwind.css") }
        document::Stylesheet { href: asset!("/src/components/alert_dialog/style.css") }
        div { class: "flex flex-col w-full h-full",
            MenuBar { menu, on_action: move |_: String| {} }
            DialogView { request }
        }
    }
}
