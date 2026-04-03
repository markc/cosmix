//! Blocking dialog invocation API for embedding in async runtimes.
//!
//! Uses a persistent GTK thread to avoid GTK's single-thread initialization
//! constraint. All dialog requests are serialized through this thread.

use std::sync::{mpsc, Mutex, OnceLock};

use crate::{DialogAction, DialogData, DialogRequest, DialogResult};

/// Message sent to the persistent GTK thread.
struct DialogJob {
    request: DialogRequest,
    reply: mpsc::Sender<DialogResult>,
}

/// The persistent GTK thread's command channel.
static GTK_THREAD: OnceLock<Mutex<mpsc::Sender<DialogJob>>> = OnceLock::new();

/// Get or create the persistent GTK thread sender.
fn gtk_sender() -> &'static Mutex<mpsc::Sender<DialogJob>> {
    GTK_THREAD.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<DialogJob>();

        std::thread::Builder::new()
            .name("cosmix-dialog-gtk".into())
            .spawn(move || {
                // GTK init happens exactly once on this thread
                #[cfg(feature = "layer-shell")]
                {
                    let _ = gtk::init();
                    super::layer_backend::init_theme();
                }

                // Process dialog requests serially
                while let Ok(job) = rx.recv() {
                    let result = run_on_gtk_thread(job.request);
                    let _ = job.reply.send(result);
                }
            })
            .expect("failed to spawn GTK thread");

        Mutex::new(tx)
    })
}

/// Show a dialog and block until the user responds.
///
/// Safe to call from any thread, any number of times. All requests are
/// serialized through a single persistent GTK thread.
pub fn run_blocking(request: DialogRequest) -> DialogResult {
    let (reply_tx, reply_rx) = mpsc::channel();

    let sender = gtk_sender().lock().expect("GTK thread sender poisoned");
    if sender
        .send(DialogJob {
            request,
            reply: reply_tx,
        })
        .is_err()
    {
        return DialogResult {
            action: DialogAction::Error("GTK thread died".into()),
            data: DialogData::None,
            rc: 10,
        };
    }
    drop(sender); // release lock before blocking

    reply_rx.recv().unwrap_or(DialogResult {
        action: DialogAction::Error("dialog thread panicked".into()),
        data: DialogData::None,
        rc: 10,
    })
}

/// Async wrapper for use in tokio contexts.
pub async fn run_async(request: DialogRequest) -> DialogResult {
    tokio::task::spawn_blocking(move || run_blocking(request))
        .await
        .unwrap_or(DialogResult {
            action: DialogAction::Error("dialog task panicked".into()),
            data: DialogData::None,
            rc: 10,
        })
}

/// Run the dialog on the persistent GTK thread.
fn run_on_gtk_thread(request: DialogRequest) -> DialogResult {
    #[cfg(feature = "layer-shell")]
    {
        let on_wayland = std::env::var("WAYLAND_DISPLAY").is_ok();
        if on_wayland && super::layer_backend::is_available() {
            return super::layer_backend::run(request);
        }
    }

    // Fallback: run as subprocess
    run_as_subprocess(request)
}

/// Fallback: invoke cosmix-dialog as a subprocess.
fn run_as_subprocess(request: DialogRequest) -> DialogResult {
    use std::process::Command;

    let mut cmd = Command::new("cosmix-dialog");

    match &request.kind {
        crate::DialogKind::Message { text, level, .. } => {
            let mode = match level {
                crate::types::MessageLevel::Info => "info",
                crate::types::MessageLevel::Warning => "warning",
                crate::types::MessageLevel::Error => "error",
            };
            cmd.args([mode, "--text", text]);
        }
        crate::DialogKind::Question {
            text,
            yes_label,
            no_label,
            cancel,
        } => {
            cmd.args(["confirm", "--text", text]);
            if let Some(y) = yes_label {
                cmd.args(["--yes-label", y]);
            }
            if let Some(n) = no_label {
                cmd.args(["--no-label", n]);
            }
            if *cancel {
                cmd.arg("--cancel");
            }
        }
        crate::DialogKind::Entry {
            text,
            default,
            placeholder,
        } => {
            cmd.args(["input", "--text", text]);
            if let Some(d) = default {
                cmd.args(["--entry-text", d]);
            }
            if let Some(p) = placeholder {
                cmd.args(["--placeholder", p]);
            }
        }
        crate::DialogKind::Password { text } => {
            cmd.args(["password", "--text", text]);
        }
        _ => {
            return DialogResult {
                action: DialogAction::Error("unsupported dialog type for subprocess".into()),
                data: DialogData::None,
                rc: 10,
            };
        }
    }

    if request.json_output {
        cmd.arg("--json");
    }

    match cmd.output() {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let rc = output.status.code().unwrap_or(10);
            let action = match rc {
                0 => DialogAction::Ok,
                1 => DialogAction::Cancel,
                5 => DialogAction::Timeout,
                _ => DialogAction::Error(format!("exit code {rc}")),
            };
            let data = if stdout.is_empty() {
                DialogData::None
            } else {
                DialogData::Text(stdout)
            };
            DialogResult { action, data, rc }
        }
        Err(e) => DialogResult {
            action: DialogAction::Error(e.to_string()),
            data: DialogData::None,
            rc: 10,
        },
    }
}
