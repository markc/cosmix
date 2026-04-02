//! Progress dialog — determinate bar or indeterminate pulsating mode.
//!
//! In CLI mode, reads percentage (0–100) from stdin line-by-line.
//! Lines starting with '#' update the status text.

use dioxus::prelude::*;

use crate::types::DialogKind;
use crate::window::{complete, exit};
use crate::{DialogAction, DialogData, DialogRequest};

#[component]
pub fn ProgressDialog(request: DialogRequest) -> Element {
    let DialogKind::Progress { ref text, pulsate, auto_close } = request.kind else {
        return rsx! {};
    };

    let initial_text = text.clone();
    let mut status = use_signal(|| initial_text);
    let mut percent = use_signal(|| 0u32);

    // Read stdin lines for progress updates via channel
    use_hook(move || {
        let (tx, rx) = std::sync::mpsc::channel::<String>();

        std::thread::spawn(move || {
            use std::io::BufRead;
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });

        spawn(async move {
            loop {
                match rx.try_recv() {
                    Ok(line) => {
                        let line = line.trim().to_string();
                        if line.starts_with('#') {
                            status.set(line[1..].trim().to_string());
                        } else if let Ok(n) = line.parse::<u32>() {
                            let n = n.min(100);
                            percent.set(n);
                            if n >= 100 && auto_close {
                                complete(DialogAction::Ok, DialogData::Number(100.0));
                                exit();
                                return;
                            }
                        }
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                }
            }
        });
    });

    let pct = *percent.read();
    let width_style = format!("width:{pct}%");

    rsx! {
        div { class: "alert-dialog",
            div { class: "alert-dialog-body-fill",
                style: "justify-content:center",
                div { class: "alert-dialog-description", "{status}" }
                div { class: "alert-dialog-progress-track",
                    if pulsate {
                        div { class: "alert-dialog-progress-pulsate" }
                    } else {
                        div {
                            class: "alert-dialog-progress-fill",
                            style: "{width_style}",
                        }
                    }
                }
                if !pulsate {
                    div {
                        class: "alert-dialog-field-help",
                        style: "text-align:right",
                        "{pct}%"
                    }
                }
            }
            div { class: "alert-dialog-actions",
                div {
                    class: "alert-dialog-cancel",
                    onclick: move |_| {
                        complete(DialogAction::Cancel, DialogData::Number(pct as f64));
                        exit();
                    },
                    "Cancel"
                }
            }
        }
    }
}
