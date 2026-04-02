//! Window creation and exit handling for dialog windows.

use std::sync::atomic::{AtomicI32, Ordering};

use dioxus::prelude::*;

use crate::{DialogAction, DialogData, DialogResult};

/// Global exit code — set before calling `exit()`.
pub static EXIT_CODE: AtomicI32 = AtomicI32::new(0);

/// Whether to output JSON (set once from main before launch).
pub static JSON_OUTPUT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Global result signal — renderers write here, app() watches and exits.
pub static RESULT: GlobalSignal<Option<DialogResult>> = Signal::global(|| None);

/// Set the dialog result and trigger exit.
pub fn complete(action: DialogAction, data: DialogData) {
    let rc = match &action {
        DialogAction::Ok | DialogAction::Yes => 0,
        DialogAction::Cancel | DialogAction::No => 1,
        DialogAction::Timeout => 5,
        DialogAction::Error(_) => 10,
        DialogAction::Custom(_) => 0,
    };
    EXIT_CODE.store(rc, Ordering::SeqCst);
    *RESULT.write() = Some(DialogResult { action, data, rc });
}

/// Exit the process after a brief delay (allows the render loop to finish).
/// Prints the result to stdout before exiting.
pub fn exit() {
    let code = EXIT_CODE.load(Ordering::SeqCst);

    // Print result to stdout before process::exit kills us
    if let Some(result) = RESULT.peek().as_ref() {
        // Check if json output was requested via the REQUEST in main
        let json = JSON_OUTPUT.load(Ordering::SeqCst);
        let output = result.to_stdout(json);
        if !output.is_empty() {
            print!("{output}");
        }
    }

    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::process::exit(code);
    });
}

/// Create desktop window config for a dialog.
#[cfg(feature = "desktop")]
pub fn dialog_window_config(title: &str, width: f64, height: f64) -> dioxus_desktop::Config {
    use dioxus_desktop::{muda::Menu, Config, LogicalSize, WindowBuilder};

    Config::new()
        .with_window(
            WindowBuilder::new()
                .with_title(title)
                .with_inner_size(LogicalSize::new(width, height))
                .with_min_inner_size(LogicalSize::new(width, height))
                .with_max_inner_size(LogicalSize::new(width, height))
                .with_resizable(false)
                .with_decorations(false),
        )
        .with_menu(Menu::new())
}
