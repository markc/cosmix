//! Thin safe wrappers around libgtk-layer-shell FFI.
//!
//! Only the functions needed for dialog overlay surfaces.
//! Linked via pkg-config in build.rs.

use gtk::prelude::*;

// ── C enums (values from gtk-layer-shell.h) ───────��──────────────────

#[repr(C)]
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub enum Layer {
    Background = 0,
    Bottom = 1,
    Top = 2,
    Overlay = 3,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub enum Edge {
    Left = 0,
    Right = 1,
    Top = 2,
    Bottom = 3,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub enum KeyboardMode {
    None = 0,
    Exclusive = 1,
    OnDemand = 2,
}

// ── Raw FFI ──────────────────────────────────────────────────────────

unsafe extern "C" {
    fn gtk_layer_is_supported() -> glib::ffi::gboolean;
    fn gtk_layer_init_for_window(window: *mut gtk::ffi::GtkWindow);
    fn gtk_layer_set_layer(window: *mut gtk::ffi::GtkWindow, layer: Layer);
    fn gtk_layer_set_anchor(window: *mut gtk::ffi::GtkWindow, edge: Edge, anchor: glib::ffi::gboolean);
    fn gtk_layer_set_margin(window: *mut gtk::ffi::GtkWindow, edge: Edge, margin: std::ffi::c_int);
    fn gtk_layer_set_keyboard_mode(window: *mut gtk::ffi::GtkWindow, mode: KeyboardMode);
    fn gtk_layer_set_namespace(window: *mut gtk::ffi::GtkWindow, ns: *const std::ffi::c_char);
    fn gtk_layer_set_exclusive_zone(window: *mut gtk::ffi::GtkWindow, zone: std::ffi::c_int);
}

// ── Helper: get raw GtkWindow pointer ────────────────────────────────

fn window_ptr(window: &gtk::Window) -> *mut gtk::ffi::GtkWindow {
    window.as_ptr() as *mut gtk::ffi::GtkWindow
}

// ── Safe wrappers ────────────────────────────────────────────────────

/// Check if the compositor supports the layer-shell protocol.
pub fn is_supported() -> bool {
    unsafe { gtk_layer_is_supported() != 0 }
}

/// Initialize a GtkWindow as a layer surface. Must be called before the window is realized.
pub fn init_for_window(window: &gtk::Window) {
    unsafe { gtk_layer_init_for_window(window_ptr(window)) }
}

pub fn set_layer(window: &gtk::Window, layer: Layer) {
    unsafe { gtk_layer_set_layer(window_ptr(window), layer) }
}

pub fn set_anchor(window: &gtk::Window, edge: Edge, anchor: bool) {
    unsafe {
        gtk_layer_set_anchor(window_ptr(window), edge, if anchor { 1 } else { 0 });
    }
}

pub fn set_margin(window: &gtk::Window, edge: Edge, margin: i32) {
    unsafe { gtk_layer_set_margin(window_ptr(window), edge, margin) }
}

pub fn set_keyboard_mode(window: &gtk::Window, mode: KeyboardMode) {
    unsafe { gtk_layer_set_keyboard_mode(window_ptr(window), mode) }
}

pub fn set_namespace(window: &gtk::Window, ns: &str) {
    let c_ns = std::ffi::CString::new(ns).expect("namespace contains null byte");
    unsafe { gtk_layer_set_namespace(window_ptr(window), c_ns.as_ptr()) }
}

pub fn set_exclusive_zone(window: &gtk::Window, zone: i32) {
    unsafe { gtk_layer_set_exclusive_zone(window_ptr(window), zone) }
}
