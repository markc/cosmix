use crate::tabs::{Cleanup, Outcome, TabSet};
use cosmix_client::{BoundedIncomingEvent, SupervisedClient};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

pub const HELP: &str = "term: tabbed Wayland Mix terminal\nDIAGNOSTIC surface — full ABP control (windows/tabs/panes/sessions per SPEC) is P3a, gated on authenticated per-instance identity (P0-I); this self-asserted `term` name is a placeholder, not the shipped multi-user identity.\nINFO / HELP\nterm.tabs: list id, active, title, cols, rows, child_pid\nterm.tab.new: open and activate a tab\nterm.tab.select: select numeric id from body\nterm.tab.close: close numeric id from body; last tab quits\nThese tab verbs are DIAGNOSTIC too; real per-instance identity is P0-I.\nterm.snapshot: read-only active screen, dimensions, cursor, child pid, byte counters and DIAGNOSTIC timings\nterm.type: DIAGNOSTIC ONLY; ASCII synthetic keys to the active tab through the keyboard encoder, max 8192 bytes; newline=Enter, tab, backspace, Ctrl+C/D supported. Not a product input API.\nDIAGNOSTIC timings are process-side, never presented-frame evidence.";
pub fn start(terminal: Arc<Mutex<TabSet>>, cleanup: Cleanup) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new().name("term-bus".into()).spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("Bus runtime");
        runtime.block_on(async move {
            let result = tokio::time::timeout(Duration::from_secs(2), SupervisedClient::connect_options("term", &cosmix_config::client_helpers::resolve_noded_url()).bounded_incoming(16).connect()).await;
            let client = match result { Ok(Ok(client)) => client, _ => { eprintln!("term Bus unavailable or connection timed out"); return; } };
            let Some(mut incoming) = client.incoming_bounded() else { return; };
            while !terminal.lock().unwrap().is_empty() {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {},
                    event = incoming.recv() => {
                        let command = match event {
                            Some(BoundedIncomingEvent::Command(c)) => c,
                            Some(BoundedIncomingEvent::Overflow { .. }) => { eprintln!("term Bus incoming overflow"); continue; },
                            None => break,
                        };
                        let result = if command.body.len() > 8192 { Err("request exceeds 8192 bytes".into()) } else {
                            handle(&terminal, &cleanup, &command.command, &command.body)
                        };
                        let (rc,body) = match result { Ok(body) => (0,body), Err(error) => (10,error) };
                        let _ = tokio::time::timeout(Duration::from_secs(2),client.respond(&command,rc,&body)).await;
                    }
                }
            }
            let _ = tokio::time::timeout(Duration::from_secs(2), client.close()).await;
        });
    }).expect("Bus thread")
}

fn handle(
    set: &Mutex<TabSet>,
    cleanup: &Cleanup,
    verb: &str,
    body: &str,
) -> Result<String, String> {
    let mut tabs = set.lock().unwrap();
    match verb {
        "INFO" | "HELP" | "info" | "help" => Ok(HELP.into()),
        "term.tabs" => Ok(tabs
            .list()
            .iter()
            .map(|tab| {
                format!(
                    "id={} active={} title={} cols={} rows={} child_pid={}",
                    tab.id, tab.active, tab.title, tab.cols, tab.rows, tab.child_pid
                )
            })
            .collect::<Vec<_>>()
            .join("\n")),
        "term.tab.new" => tabs.open().map(|id| format!("opened id={id}")),
        "term.tab.select" => {
            let id = parse_id(body)?;
            // select wakes the event loop; refresh compares View.rendered_id
            // with active_id and uploads even without a PTY damage event.
            if tabs.select(id) {
                Ok(format!("selected id={id}"))
            } else {
                Err(format!("unknown tab id={id}"))
            }
        }
        "term.tab.close" => {
            let id = parse_id(body)?;
            let (outcome, removed) = tabs.close(id);
            drop(tabs);
            cleanup.submit(removed.into_iter().collect());
            match outcome {
                Outcome::Unknown => Err(format!("unknown tab id={id}")),
                Outcome::Remaining(count) => Ok(format!("closed id={id} remaining={count}")),
                Outcome::Empty => Ok(format!("closed id={id} last")),
            }
        }
        "term.snapshot" | "term.type" => {
            if tabs.is_empty() {
                return Err("application closing".into());
            }
            let active = tabs.active_terminal();
            let terminal = active.lock().unwrap();
            if verb == "term.snapshot" {
                Ok(terminal.snapshot())
            } else {
                terminal.listener.type_text(body).map(|_| {
                    "DIAGNOSTIC synthetic keys queued; inspect input_written for actual writes"
                        .into()
                })
            }
        }
        _ => Err("unknown verb; use HELP".into()),
    }
}

fn parse_id(body: &str) -> Result<u64, String> {
    let body = body.trim();
    if body.is_empty() || !body.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("body must be a numeric tab id".into());
    }
    body.parse()
        .map_err(|_| "body must be a numeric tab id".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn numeric_body_only() {
        assert_eq!(parse_id("42\n"), Ok(42));
        for body in ["", "id=42", "-1", "+1", "1 2", "18446744073709551616"] {
            assert!(parse_id(body).is_err());
        }
    }
}
