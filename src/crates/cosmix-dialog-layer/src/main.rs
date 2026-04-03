//! POC: GTK layer-shell dialog that bypasses cosmic-comp's 240px minimum.
//!
//! Creates a 320x100 overlay dialog with a message and OK button.
//! Proves layer surfaces are not subject to toplevel min_size enforcement.

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod layer_shell;

use std::cell::Cell;
use std::rc::Rc;

use gdk::keys::constants as key;
use gtk::prelude::*;

const WIDTH: i32 = 320;
const HEIGHT: i32 = 100;

const CSS: &str = r#"
window {
    background-color: #030712;
    color: #f3f4f6;
    font-family: system-ui, sans-serif;
    font-size: 14px;
}
.dialog-label {
    padding: 0.75rem 1rem;
    font-size: 0.9375rem;
}
.dialog-footer {
    padding: 0.5rem 1rem;
    background-color: #111827;
    border-top: 1px solid rgba(128, 128, 128, 0.25);
}
.dialog-btn {
    padding: 0.375rem 1.25rem;
    border-radius: 0.375rem;
    background-color: #3b82f6;
    color: #ffffff;
    font-weight: 500;
    font-size: 0.875rem;
    border: none;
}
.dialog-btn:hover {
    background-color: #2563eb;
}
"#;

fn main() {
    gtk::init().expect("GTK init failed");

    // Check layer-shell support
    if !layer_shell::is_supported() {
        eprintln!("error: compositor does not support wlr-layer-shell");
        std::process::exit(10);
    }

    // Load CSS
    let provider = gtk::CssProvider::new();
    provider.load_from_data(CSS.as_bytes()).expect("CSS load failed");
    gtk::StyleContext::add_provider_for_screen(
        &gdk::Screen::default().expect("no default screen"),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    // Create window
    let window = gtk::Window::new(gtk::WindowType::Toplevel);

    // Configure as layer surface — BEFORE realize/show
    layer_shell::init_for_window(&window);
    layer_shell::set_layer(&window, layer_shell::Layer::Overlay);
    layer_shell::set_keyboard_mode(&window, layer_shell::KeyboardMode::OnDemand);
    layer_shell::set_namespace(&window, "cosmix-dialog");
    layer_shell::set_exclusive_zone(&window, -1); // don't reserve space

    // No anchors = centered by compositor (default behavior)

    // Force exact size
    window.set_default_size(WIDTH, HEIGHT);
    window.set_size_request(WIDTH, HEIGHT);

    // Exit code tracked via Rc<Cell>
    let exit_code = Rc::new(Cell::new(1i32)); // default: cancel

    // ── Widget tree ──────────────────────────────────────────────

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);

    // Message label (fills available space)
    let label = gtk::Label::new(Some("Hello from layer-shell! This window is 100px tall."));
    label.set_line_wrap(true);
    label.style_context().add_class("dialog-label");
    label.set_valign(gtk::Align::Center);
    label.set_vexpand(true);

    // Footer with OK button
    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    footer.style_context().add_class("dialog-footer");

    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    footer.add(&spacer);

    let ok_btn = gtk::Button::with_label("OK");
    ok_btn.style_context().add_class("dialog-btn");
    let ec = exit_code.clone();
    ok_btn.connect_clicked(move |_| {
        ec.set(0);
        gtk::main_quit();
    });
    footer.add(&ok_btn);

    vbox.add(&label);
    vbox.pack_end(&footer, false, false, 0);

    window.add(&vbox);

    // ── Key handling ─────────────────────────────────────────────

    let ec = exit_code.clone();
    window.connect_key_press_event(move |_, event| {
        match event.keyval() {
            key::Escape => {
                ec.set(1);
                gtk::main_quit();
                glib::Propagation::Stop
            }
            key::Return | key::KP_Enter => {
                ec.set(0);
                gtk::main_quit();
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        }
    });

    // Close button / compositor close
    let ec = exit_code.clone();
    window.connect_delete_event(move |_, _| {
        ec.set(1);
        gtk::main_quit();
        glib::Propagation::Stop
    });

    window.show_all();
    gtk::main();

    std::process::exit(exit_code.get());
}
