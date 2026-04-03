//! GTK widget builders for compact dialog types rendered via layer-shell.
//!
//! Each builder creates a widget tree for a specific DialogKind variant.
//! Returns a GtkBox that gets packed into the layer-shell window.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;

use crate::types::*;
use crate::{DialogAction, DialogData, DialogResult};

/// Shared state for the dialog result, set by widget callbacks.
pub struct DialogState {
    pub result: RefCell<Option<DialogResult>>,
}

impl DialogState {
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            result: RefCell::new(None),
        })
    }

    pub fn complete(&self, action: DialogAction, data: DialogData) {
        let rc = match &action {
            DialogAction::Ok | DialogAction::Yes => 0,
            DialogAction::Cancel | DialogAction::No => 1,
            DialogAction::Timeout => 5,
            DialogAction::Error(_) => 10,
            DialogAction::Custom(_) => 0,
        };
        *self.result.borrow_mut() = Some(DialogResult { action, data, rc });
        gtk::main_quit();
    }
}

// ── Message dialog ───────────────────────────────────────────────────

pub fn build_message(
    text: &str,
    level: &MessageLevel,
    detail: Option<&str>,
    state: &Rc<DialogState>,
) -> gtk::Box {
    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);

    // Body
    let body = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    body.style_context().add_class("dialog-body");
    body.set_valign(gtk::Align::Center);
    body.set_vexpand(true);

    // Icon
    let icon_name = match level {
        MessageLevel::Info => "dialog-information",
        MessageLevel::Warning => "dialog-warning",
        MessageLevel::Error => "dialog-error",
    };
    let icon = gtk::Image::from_icon_name(Some(icon_name), gtk::IconSize::Dialog);
    let icon_class = match level {
        MessageLevel::Info => "icon-info",
        MessageLevel::Warning => "icon-warning",
        MessageLevel::Error => "icon-error",
    };
    icon.style_context().add_class(icon_class);
    body.add(&icon);

    // Text column
    let text_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    let label = gtk::Label::new(Some(text));
    label.set_line_wrap(true);
    label.set_xalign(0.0);
    label.style_context().add_class("dialog-label");
    text_box.add(&label);

    if let Some(detail_text) = detail {
        let detail_label = gtk::Label::new(Some(detail_text));
        detail_label.set_line_wrap(true);
        detail_label.set_xalign(0.0);
        detail_label.style_context().add_class("dialog-detail");
        text_box.add(&detail_label);
    }
    body.add(&text_box);

    // Footer
    let footer = build_footer();
    let ok_btn = gtk::Button::with_label("OK");
    ok_btn.style_context().add_class("btn-primary");
    let s = state.clone();
    ok_btn.connect_clicked(move |_| s.complete(DialogAction::Ok, DialogData::None));
    footer.pack_end(&ok_btn, false, false, 0);

    vbox.add(&body);
    vbox.pack_end(&footer, false, false, 0);
    vbox
}

// ── Question dialog ──────────────────────────────────────────────────

pub fn build_question(
    text: &str,
    yes_label: Option<&str>,
    no_label: Option<&str>,
    cancel: bool,
    state: &Rc<DialogState>,
) -> gtk::Box {
    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);

    let body = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    body.style_context().add_class("dialog-body");
    body.set_valign(gtk::Align::Center);
    body.set_vexpand(true);

    let icon = gtk::Image::from_icon_name(Some("dialog-question"), gtk::IconSize::Dialog);
    icon.style_context().add_class("icon-info");
    body.add(&icon);

    let label = gtk::Label::new(Some(text));
    label.set_line_wrap(true);
    label.set_xalign(0.0);
    label.style_context().add_class("dialog-label");
    body.add(&label);

    let footer = build_footer();

    if cancel {
        let cancel_btn = gtk::Button::with_label("Cancel");
        cancel_btn.style_context().add_class("btn-secondary");
        let s = state.clone();
        cancel_btn.connect_clicked(move |_| s.complete(DialogAction::Cancel, DialogData::None));
        footer.pack_start(&cancel_btn, false, false, 0);
    }

    let no_btn = gtk::Button::with_label(no_label.unwrap_or("No"));
    no_btn.style_context().add_class("btn-secondary");
    let s = state.clone();
    no_btn.connect_clicked(move |_| s.complete(DialogAction::No, DialogData::Bool(false)));
    footer.pack_end(&no_btn, false, false, 0);

    let yes_btn = gtk::Button::with_label(yes_label.unwrap_or("Yes"));
    yes_btn.style_context().add_class("btn-primary");
    let s = state.clone();
    yes_btn.connect_clicked(move |_| s.complete(DialogAction::Yes, DialogData::Bool(true)));
    footer.pack_end(&yes_btn, false, false, 4);

    vbox.add(&body);
    vbox.pack_end(&footer, false, false, 0);
    vbox
}

// ── Entry dialog ─────────────────────────────────────────────────────

pub fn build_entry(
    text: &str,
    default: Option<&str>,
    placeholder: Option<&str>,
    state: &Rc<DialogState>,
) -> gtk::Box {
    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);

    let body = gtk::Box::new(gtk::Orientation::Vertical, 6);
    body.style_context().add_class("dialog-body");
    body.set_valign(gtk::Align::Center);
    body.set_vexpand(true);

    let label = gtk::Label::new(Some(text));
    label.set_line_wrap(true);
    label.set_xalign(0.0);
    label.style_context().add_class("dialog-label");
    body.add(&label);

    let entry = gtk::Entry::new();
    if let Some(d) = default {
        entry.set_text(d);
    }
    if let Some(p) = placeholder {
        entry.set_placeholder_text(Some(p));
    }
    entry.set_activates_default(true);
    body.add(&entry);

    let footer = build_footer();

    let cancel_btn = gtk::Button::with_label("Cancel");
    cancel_btn.style_context().add_class("btn-secondary");
    let s = state.clone();
    cancel_btn.connect_clicked(move |_| s.complete(DialogAction::Cancel, DialogData::None));
    footer.pack_start(&cancel_btn, false, false, 0);

    let ok_btn = gtk::Button::with_label("OK");
    ok_btn.style_context().add_class("btn-primary");
    let entry_clone = entry.clone();
    let s = state.clone();
    ok_btn.connect_clicked(move |_| {
        let text = entry_clone.text().to_string();
        s.complete(DialogAction::Ok, DialogData::Text(text));
    });
    footer.pack_end(&ok_btn, false, false, 0);

    // Enter key in entry submits
    let entry_clone2 = entry.clone();
    let s = state.clone();
    entry.connect_activate(move |_| {
        let text = entry_clone2.text().to_string();
        s.complete(DialogAction::Ok, DialogData::Text(text));
    });

    vbox.add(&body);
    vbox.pack_end(&footer, false, false, 0);
    vbox
}

// ── Password dialog ──────────────────────────────────────────────────

pub fn build_password(text: &str, state: &Rc<DialogState>) -> gtk::Box {
    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);

    let body = gtk::Box::new(gtk::Orientation::Vertical, 6);
    body.style_context().add_class("dialog-body");
    body.set_valign(gtk::Align::Center);
    body.set_vexpand(true);

    let label = gtk::Label::new(Some(text));
    label.set_line_wrap(true);
    label.set_xalign(0.0);
    label.style_context().add_class("dialog-label");
    body.add(&label);

    let entry = gtk::Entry::new();
    entry.set_visibility(false);
    entry.set_input_purpose(gtk::InputPurpose::Password);
    entry.set_activates_default(true);
    body.add(&entry);

    let footer = build_footer();

    let cancel_btn = gtk::Button::with_label("Cancel");
    cancel_btn.style_context().add_class("btn-secondary");
    let s = state.clone();
    cancel_btn.connect_clicked(move |_| s.complete(DialogAction::Cancel, DialogData::None));
    footer.pack_start(&cancel_btn, false, false, 0);

    let ok_btn = gtk::Button::with_label("OK");
    ok_btn.style_context().add_class("btn-primary");
    let entry_clone = entry.clone();
    let s = state.clone();
    ok_btn.connect_clicked(move |_| {
        let text = entry_clone.text().to_string();
        s.complete(DialogAction::Ok, DialogData::Text(text));
    });
    footer.pack_end(&ok_btn, false, false, 0);

    let entry_clone2 = entry.clone();
    let s = state.clone();
    entry.connect_activate(move |_| {
        let text = entry_clone2.text().to_string();
        s.complete(DialogAction::Ok, DialogData::Text(text));
    });

    vbox.add(&body);
    vbox.pack_end(&footer, false, false, 0);
    vbox
}

// ── ComboBox dialog ──────────────────────────────────────────────────

pub fn build_combobox(
    text: &str,
    items: &[String],
    default: Option<usize>,
    editable: bool,
    state: &Rc<DialogState>,
) -> gtk::Box {
    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);

    let body = gtk::Box::new(gtk::Orientation::Vertical, 6);
    body.style_context().add_class("dialog-body");
    body.set_valign(gtk::Align::Center);
    body.set_vexpand(true);

    let label = gtk::Label::new(Some(text));
    label.set_line_wrap(true);
    label.set_xalign(0.0);
    label.style_context().add_class("dialog-label");
    body.add(&label);

    let combo = if editable {
        let c = gtk::ComboBoxText::with_entry();
        for item in items {
            c.append_text(item);
        }
        if let Some(idx) = default {
            c.set_active(Some(idx as u32));
        }
        c
    } else {
        let c = gtk::ComboBoxText::new();
        for item in items {
            c.append_text(item);
        }
        c.set_active(Some(default.unwrap_or(0) as u32));
        c
    };
    body.add(&combo);

    let footer = build_footer();

    let cancel_btn = gtk::Button::with_label("Cancel");
    cancel_btn.style_context().add_class("btn-secondary");
    let s = state.clone();
    cancel_btn.connect_clicked(move |_| s.complete(DialogAction::Cancel, DialogData::None));
    footer.pack_start(&cancel_btn, false, false, 0);

    let ok_btn = gtk::Button::with_label("OK");
    ok_btn.style_context().add_class("btn-primary");
    let combo_clone = combo.clone();
    let s = state.clone();
    ok_btn.connect_clicked(move |_| {
        let text = combo_clone
            .active_text()
            .map(|t| t.to_string())
            .unwrap_or_default();
        s.complete(DialogAction::Ok, DialogData::Text(text));
    });
    footer.pack_end(&ok_btn, false, false, 0);

    vbox.add(&body);
    vbox.pack_end(&footer, false, false, 0);
    vbox
}

// ── Progress dialog ──────────────────────────────────────────────────

pub fn build_progress(
    text: &str,
    pulsate: bool,
    auto_close: bool,
    state: &Rc<DialogState>,
) -> gtk::Box {
    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);

    let body = gtk::Box::new(gtk::Orientation::Vertical, 6);
    body.style_context().add_class("dialog-body");
    body.set_valign(gtk::Align::Center);
    body.set_vexpand(true);

    let label = gtk::Label::new(Some(text));
    label.set_line_wrap(true);
    label.set_xalign(0.0);
    label.style_context().add_class("dialog-label");
    body.add(&label);

    let progress = gtk::ProgressBar::new();
    progress.set_show_text(true);

    if pulsate {
        progress.pulse();
        let p = progress.clone();
        glib::timeout_add_local(std::time::Duration::from_millis(100), move || {
            p.pulse();
            glib::ControlFlow::Continue
        });
    }
    body.add(&progress);

    // Read stdin for percentage updates in a background thread
    let (tx, rx) = glib::MainContext::channel::<String>(glib::Priority::DEFAULT);
    std::thread::spawn(move || {
        use std::io::BufRead;
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(l) => {
                    if tx.send(l).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let p = progress.clone();
    let s = state.clone();
    let auto_close = auto_close;
    rx.attach(None, move |line| {
        if let Ok(pct) = line.trim().trim_end_matches('%').parse::<f64>() {
            let fraction = (pct / 100.0).clamp(0.0, 1.0);
            p.set_fraction(fraction);
            p.set_text(Some(&format!("{:.0}%", pct)));
            if auto_close && fraction >= 1.0 {
                s.complete(DialogAction::Ok, DialogData::None);
                return glib::ControlFlow::Break;
            }
        }
        glib::ControlFlow::Continue
    });

    let footer = build_footer();
    let cancel_btn = gtk::Button::with_label("Cancel");
    cancel_btn.style_context().add_class("btn-secondary");
    let s = state.clone();
    cancel_btn.connect_clicked(move |_| s.complete(DialogAction::Cancel, DialogData::None));
    footer.pack_end(&cancel_btn, false, false, 0);

    vbox.add(&body);
    vbox.pack_end(&footer, false, false, 0);
    vbox
}

// ── Helpers ──────────────────────────────────────────────────────────

fn build_footer() -> gtk::Box {
    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    footer.style_context().add_class("dialog-footer");
    footer
}
