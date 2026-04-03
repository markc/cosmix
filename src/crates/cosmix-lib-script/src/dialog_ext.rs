//! Mix extension functions for dialog invocation.
//!
//! Registers `dialog_info`, `dialog_confirm`, `dialog_entry`, and `dialog_password`
//! as Mix builtin functions. Each function blocks the script until the user responds.
//!
//! # Usage in Mix scripts
//!
//! ```mix
//! dialog_info("Build complete!")
//! $proceed = dialog_confirm("Deploy to production?")
//! $name = dialog_entry("What is your name?", "World")
//! $pass = dialog_password("Enter API key:")
//! ```

use std::future::Future;
use std::pin::Pin;

use mix_core::error::MixResult;
use mix_core::evaluator::{Evaluator, ExtFn};
use mix_core::value::Value;

use cosmix_dialog::backend::blocking;
use cosmix_dialog::types::MessageLevel;
use cosmix_dialog::{DialogAction, DialogData, DialogKind, DialogRequest};

/// Register all dialog extension functions on a Mix evaluator.
pub fn register(eval: &mut Evaluator) {
    eval.register("dialog_info", make_dialog_info());
    eval.register("dialog_warning", make_dialog_warning());
    eval.register("dialog_error", make_dialog_error());
    eval.register("dialog_confirm", make_dialog_confirm());
    eval.register("dialog_entry", make_dialog_entry());
    eval.register("dialog_password", make_dialog_password());
}

/// Helper: create a DialogRequest, run it blocking, return the result.
fn show_dialog(request: DialogRequest) -> DialogResult {
    // Spawn GTK on a dedicated thread and block until the dialog completes.
    // This is safe to call from an async context because we use std::thread,
    // not tokio::task — the async runtime is not blocked.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = blocking::run_blocking(request);
        let _ = tx.send(result);
    });
    rx.recv().unwrap_or(cosmix_dialog::DialogResult {
        action: DialogAction::Error("dialog thread panicked".into()),
        data: DialogData::None,
        rc: 10,
    })
}

use cosmix_dialog::DialogResult;

// ── dialog_info(text) ───���────────────────────────────────────────────

fn make_dialog_info() -> ExtFn {
    make_message_fn(MessageLevel::Info)
}

fn make_dialog_warning() -> ExtFn {
    make_message_fn(MessageLevel::Warning)
}

fn make_dialog_error() -> ExtFn {
    make_message_fn(MessageLevel::Error)
}

fn make_message_fn(level: MessageLevel) -> ExtFn {
    Box::new(move |args: Vec<Value>| -> Pin<Box<dyn Future<Output = MixResult<Value>>>> {
        let text = args.first().map(|v| v.to_string()).unwrap_or_default();
        let request = DialogRequest {
            kind: DialogKind::Message {
                text,
                level,
                detail: None,
            },
            title: None,
            size: None,
            timeout: 0,
            json_output: false,
        };
        Box::pin(async move {
            // Run on a background thread to avoid blocking the async runtime
            let result = tokio::task::spawn_blocking(move || blocking::run_blocking(request))
                .await
                .unwrap_or(DialogResult {
                    action: DialogAction::Ok,
                    data: DialogData::None,
                    rc: 0,
                });
            Ok(Value::Nil)
        })
    })
}

// ── dialog_confirm(text) → bool ────────────────────────────────────��─

fn make_dialog_confirm() -> ExtFn {
    Box::new(|args: Vec<Value>| -> Pin<Box<dyn Future<Output = MixResult<Value>>>> {
        let text = args.first().map(|v| v.to_string()).unwrap_or_default();
        let request = DialogRequest {
            kind: DialogKind::Question {
                text,
                yes_label: None,
                no_label: None,
                cancel: false,
            },
            title: None,
            size: None,
            timeout: 0,
            json_output: false,
        };
        Box::pin(async move {
            let result = tokio::task::spawn_blocking(move || blocking::run_blocking(request))
                .await
                .unwrap_or(DialogResult {
                    action: DialogAction::No,
                    data: DialogData::Bool(false),
                    rc: 1,
                });
            Ok(Value::Bool(result.rc == 0))
        })
    })
}

// ── dialog_entry(text, [default]) → string ───────────────────────────

fn make_dialog_entry() -> ExtFn {
    Box::new(|args: Vec<Value>| -> Pin<Box<dyn Future<Output = MixResult<Value>>>> {
        let text = args.first().map(|v| v.to_string()).unwrap_or_default();
        let default = args.get(1).map(|v| v.to_string());
        let request = DialogRequest {
            kind: DialogKind::Entry {
                text,
                default,
                placeholder: None,
            },
            title: None,
            size: None,
            timeout: 0,
            json_output: false,
        };
        Box::pin(async move {
            let result = tokio::task::spawn_blocking(move || blocking::run_blocking(request))
                .await
                .unwrap_or(DialogResult {
                    action: DialogAction::Cancel,
                    data: DialogData::None,
                    rc: 1,
                });
            match result.data {
                DialogData::Text(s) => Ok(Value::String(s)),
                _ if result.rc == 0 => Ok(Value::String(String::new())),
                _ => Ok(Value::Nil),
            }
        })
    })
}

// ── dialog_password(text) → string ───────────────────────────────────

fn make_dialog_password() -> ExtFn {
    Box::new(|args: Vec<Value>| -> Pin<Box<dyn Future<Output = MixResult<Value>>>> {
        let text = args.first().map(|v| v.to_string()).unwrap_or_default();
        let request = DialogRequest {
            kind: DialogKind::Password { text },
            title: None,
            size: None,
            timeout: 0,
            json_output: false,
        };
        Box::pin(async move {
            let result = tokio::task::spawn_blocking(move || blocking::run_blocking(request))
                .await
                .unwrap_or(DialogResult {
                    action: DialogAction::Cancel,
                    data: DialogData::None,
                    rc: 1,
                });
            match result.data {
                DialogData::Text(s) => Ok(Value::String(s)),
                _ if result.rc == 0 => Ok(Value::String(String::new())),
                _ => Ok(Value::Nil),
            }
        })
    })
}
