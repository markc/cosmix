//! Layer-shell backend — compact native GTK dialogs via wlr-layer-shell.
//!
//! Bypasses cosmic-comp's 240px toplevel minimum by rendering as an overlay
//! layer surface with a native GTK widget tree (no WebKitGTK).

use std::rc::Rc;

use gdk::keys::constants as key;
use gtk::prelude::*;

use crate::layer::{ffi, theme};
use crate::layer::widgets::{self, DialogState};
use crate::{DialogAction, DialogData, DialogKind, DialogRequest, DialogResult};

/// Check if layer-shell is available on this compositor.
/// GTK must already be initialized before calling this.
pub fn is_available() -> bool {
    ffi::is_supported()
}

/// Apply the dialog theme CSS. Idempotent — safe to call multiple times.
pub fn init_theme() {
    use std::sync::Once;
    static THEME_INIT: Once = Once::new();
    THEME_INIT.call_once(|| {
        theme::apply_theme();
    });
}

/// Run a dialog using the layer-shell backend.
/// Returns the DialogResult after the user interacts.
/// GTK must already be initialized (either by the persistent thread or by main).
pub fn run(request: DialogRequest) -> DialogResult {
    // GTK init is idempotent — safe to call multiple times on the same thread
    let _ = gtk::init();
    init_theme();

    let (w, h) = request.default_size();
    let state = DialogState::new();

    // Build the widget tree for this dialog type
    let content = build_content(&request.kind, &state);

    // Create layer-shell window
    let window = gtk::Window::new(gtk::WindowType::Toplevel);

    // Enable RGBA visual for transparent background (rounded corners)
    if let Some(screen) = gtk::prelude::WidgetExt::screen(&window) {
        if let Some(visual) = screen.rgba_visual() {
            window.set_visual(Some(&visual));
        }
    }
    window.set_app_paintable(true);

    ffi::init_for_window(&window);
    ffi::set_layer(&window, ffi::Layer::Overlay);
    ffi::set_keyboard_mode(&window, ffi::KeyboardMode::OnDemand);
    ffi::set_namespace(&window, "cosmix-dialog");
    ffi::set_exclusive_zone(&window, -1);

    // Position: centered by default (no anchors), or explicit position
    // No anchors = compositor centers the surface

    window.set_default_size(w as i32, h as i32);
    window.set_size_request(w as i32, h as i32);

    // Wrap content in a rounded frame container
    let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
    frame.style_context().add_class("dialog-frame");
    frame.add(&content);
    window.add(&frame);

    // Global key handling
    let s = state.clone();
    window.connect_key_press_event(move |_, event| {
        if event.keyval() == key::Escape {
            s.complete(DialogAction::Cancel, DialogData::None);
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });

    // Window close
    let s = state.clone();
    window.connect_delete_event(move |_, _| {
        s.complete(DialogAction::Cancel, DialogData::None);
        glib::Propagation::Stop
    });

    window.show_all();
    gtk::main();

    // Extract result
    state
        .result
        .borrow()
        .clone()
        .unwrap_or(DialogResult {
            action: DialogAction::Cancel,
            data: DialogData::None,
            rc: 1,
        })
}

/// Build the widget tree for a given dialog kind.
fn build_content(kind: &DialogKind, state: &Rc<DialogState>) -> gtk::Box {
    match kind {
        DialogKind::Message { text, level, detail } => {
            widgets::build_message(text, level, detail.as_deref(), state)
        }
        DialogKind::Question {
            text,
            yes_label,
            no_label,
            cancel,
        } => widgets::build_question(
            text,
            yes_label.as_deref(),
            no_label.as_deref(),
            *cancel,
            state,
        ),
        DialogKind::Entry {
            text,
            default,
            placeholder,
        } => widgets::build_entry(text, default.as_deref(), placeholder.as_deref(), state),
        DialogKind::Password { text } => widgets::build_password(text, state),
        DialogKind::ComboBox {
            text,
            items,
            default,
            editable,
        } => widgets::build_combobox(text, items, *default, *editable, state),
        DialogKind::Progress {
            text,
            pulsate,
            auto_close,
        } => widgets::build_progress(text, *pulsate, *auto_close, state),
        // All other types should not reach here (backend selection ensures this)
        _ => {
            let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
            let label = gtk::Label::new(Some("Unsupported dialog type for layer-shell backend"));
            vbox.add(&label);
            vbox
        }
    }
}
