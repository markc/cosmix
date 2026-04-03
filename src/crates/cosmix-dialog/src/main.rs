//! cosmix-dialog — GUI dialog utility for the Cosmix stack.
//!
//! Usage:
//!   cosmix-dialog info --text="Hello world"
//!   cosmix-dialog confirm --text="Continue?"
//!   cosmix-dialog input --text="Name:" --entry-text="Mark"
//!   cosmix-dialog password --text="Secret:"
//!   echo "log text" | cosmix-dialog text-info
//!
//! Backends:
//!   --backend=auto    Auto-select (default): layer-shell for compact, Dioxus for complex
//!   --backend=dioxus  Force Dioxus Desktop (WebKitGTK)
//!   --backend=layer   Force GTK layer-shell

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod components;

use clap::Parser;

use cosmix_dialog::backend::{self, BackendKind};
use cosmix_dialog::cli::Cli;

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

    let backend_override = cli.backend;
    let request = cli.into_request(stdin_text);
    let json_output = request.json_output;

    let backend = backend::select_backend(&request, backend_override);

    match backend {
        #[cfg(feature = "layer-shell")]
        BackendKind::LayerShell => {
            let result = backend::layer_backend::run(request);
            let output = result.to_stdout(json_output);
            if !output.is_empty() {
                print!("{output}");
            }
            std::process::exit(result.rc);
        }
        #[cfg(feature = "desktop")]
        BackendKind::Dioxus => {
            backend::dioxus_backend::run(request);
        }
    }
}
