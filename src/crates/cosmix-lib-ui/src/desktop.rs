use dioxus::prelude::*;

/// Set Linux-specific environment variables for WebKitGTK.
/// Call before Dioxus launch.
pub fn init_linux_env() {
    #[cfg(target_os = "linux")]
    unsafe {
        std::env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
    }
}

/// Standard frameless window configuration for cosmix apps.
///
/// - No compositor decorations (CSD via MenuBar caption buttons instead)
/// - No system menu bar (replaced by MenuBar component)
/// - Drag-to-move and caption buttons provided by the MenuBar component
///
/// All cosmix Dioxus desktop apps should use this instead of building Config manually.
pub fn window_config(title: &str, width: f64, height: f64) -> dioxus_desktop::Config {
    use dioxus_desktop::{muda::Menu, Config, LogicalSize, WindowBuilder};

    Config::new()
        .with_window(
            WindowBuilder::new()
                .with_title(title)
                .with_inner_size(LogicalSize::new(width, height))
                .with_decorations(false),
        )
        .with_menu(Menu::new())
}

/// Open an async file picker dialog. Returns the selected path, if any.
pub async fn pick_file(filters: &[(&str, &[&str])]) -> Option<std::path::PathBuf> {
    let mut dialog = rfd::AsyncFileDialog::new().set_title("Open file");
    for (name, exts) in filters {
        dialog = dialog.add_filter(*name, exts);
    }
    dialog = dialog.add_filter("All files", &["*"]);
    dialog.pick_file().await.map(|h| h.path().to_path_buf())
}

/// Handle Ctrl+O / Ctrl+Q keyboard shortcuts.
/// Returns true if the event was handled.
pub fn handle_shortcut(e: &KeyboardEvent, on_open: impl FnOnce(), on_quit: impl FnOnce()) -> bool {
    if e.modifiers().ctrl() {
        match e.key() {
            Key::Character(ref c) if c == "o" => { on_open(); true }
            Key::Character(ref c) if c == "q" => { on_quit(); true }
            _ => false,
        }
    } else {
        false
    }
}
