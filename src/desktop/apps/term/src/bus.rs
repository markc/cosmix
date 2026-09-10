use crate::tabs::{Cleanup, Outcome, TabSet};
use cosmix_client::{BoundedIncomingEvent, SupervisedClient};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

pub const HELP: &str = "term: tabbed Wayland Mix terminal\nDIAGNOSTIC surface — full ABP control (windows/tabs/panes/sessions per SPEC) is P3a, gated on authenticated per-instance identity (P0-I); this self-asserted `term` name is a placeholder, not the shipped multi-user identity.\nINFO / HELP\nterm.tabs: list id, active, title, cols, rows, child_pid\nterm.tab.new: open and activate a tab\nterm.tab.select: select numeric id from body\nterm.tab.close: close numeric id from body; last tab quits\nThese tab verbs are DIAGNOSTIC too; real per-instance identity is P0-I.\nterm.panes: list active tab pane ids, focus, dimensions, child pids and logical geometry\nterm.pane.split: body h|horizontal|v|vertical\nterm.pane.close: close active pane; last pane closes tab\nterm.pane.select: select numeric pane id in active tab\nThese pane verbs are DIAGNOSTIC too; real per-instance identity is P0-I.\nterm.snapshot: read-only active screen, dimensions, cursor, child pid, byte counters and DIAGNOSTIC timings\nterm.type: DIAGNOSTIC ONLY; ASCII synthetic keys to the active pane through the keyboard encoder, max 8192 bytes; newline=Enter, tab, backspace, Ctrl+C/D supported. Not a product input API.\nDIAGNOSTIC timings are process-side, never presented-frame evidence.";
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
        "term.panes" => Ok(tabs
            .leaves()
            .iter()
            .map(|pane| {
                let g = pane.geometry;
                format!(
                    "id={} active={} cols={} rows={} child_pid={} x={} y={} w={} h={}",
                    pane.id, pane.active, pane.cols, pane.rows, pane.child_pid, g.x, g.y, g.w, g.h
                )
            })
            .collect::<Vec<_>>()
            .join("\n")),
        // VERIFY: term.pane.split handler — body-only direction, new active ID.
        "term.pane.split" => {
            let dir = parse_dir(body)?;
            tabs.split_active(dir).map(|id| {
                format!(
                    "split id={id} dir={}",
                    if dir == crate::panes::SplitDir::Horizontal {
                        "h"
                    } else {
                        "v"
                    }
                )
            })
        }
        "term.pane.select" => {
            let id = parse_pane_id(body)?;
            if tabs.focus(id) {
                Ok(format!("selected id={id}"))
            } else {
                Err(format!("unknown pane in active tab id={id}"))
            }
        }
        "term.pane.close" => {
            if tabs.is_empty() {
                return Err("application closing".into());
            }
            let id = tabs.active_tab().active_pane;
            let tab_closed = tabs.leaves().len() == 1;
            let (_, removed) = tabs.close_active();
            let count = tabs.leaves().len();
            drop(tabs);
            cleanup.submit(removed.into_iter().collect());
            if tab_closed {
                Ok(format!("closed id={id} tab-closed"))
            } else {
                Ok(format!("closed id={id} panes={count}"))
            }
        }
        // VERIFY: active-pane snapshot/type — selection stays under the set lock.
        "term.snapshot" | "term.type" => {
            if tabs.is_empty() {
                return Err("application closing".into());
            }
            let active = tabs.active_pane_terminal();
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

fn parse_dir(body: &str) -> Result<crate::panes::SplitDir, String> {
    match body.trim_end_matches(['\r', '\n']) {
        "h" | "horizontal" => Ok(crate::panes::SplitDir::Horizontal),
        "v" | "vertical" => Ok(crate::panes::SplitDir::Vertical),
        _ => Err("body must be h|horizontal|v|vertical".into()),
    }
}

fn parse_pane_id(body: &str) -> Result<u64, String> {
    let body = body.trim_end_matches(['\r', '\n']);
    if body.is_empty() || !body.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("body must be a numeric pane id".into());
    }
    body.parse()
        .map_err(|_| "body must be a numeric pane id".into())
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
    fn pane_body_parsers() {
        assert_eq!(parse_pane_id("42\n"), Ok(42));
        for body in [
            "",
            "id=42",
            "-1",
            "+1",
            "1 2",
            " 42",
            "42 ",
            "18446744073709551616",
        ] {
            assert!(parse_pane_id(body).is_err(), "{body:?}");
        }
        assert_eq!(parse_dir("h\n"), Ok(crate::panes::SplitDir::Horizontal));
        assert_eq!(
            parse_dir("vertical\n"),
            Ok(crate::panes::SplitDir::Vertical)
        );
        for body in ["", "sideways", "v extra", " h"] {
            assert!(parse_dir(body).is_err());
        }
    }
    #[test]
    fn pane_handlers_and_active_snapshot() {
        if !std::path::Path::new("/opt/cosmix/bin/mix").is_file() {
            eprintln!("SKIP pane Bus test: Mix unavailable");
            return;
        }
        let set = Mutex::new(TabSet::new().unwrap());
        let (cleanup, worker) = Cleanup::start().unwrap();
        let original = set.lock().unwrap().active_tab().active_pane;
        assert_eq!(
            handle(&set, &cleanup, "term.pane.split", "v\n").unwrap(),
            "split id=2 dir=v"
        );
        assert_eq!(
            handle(&set, &cleanup, "term.panes", "")
                .unwrap()
                .lines()
                .count(),
            2
        );
        let pid = set
            .lock()
            .unwrap()
            .active_pane_terminal()
            .lock()
            .unwrap()
            .pid;
        assert!(
            handle(&set, &cleanup, "term.snapshot", "")
                .unwrap()
                .contains(&format!("child_pid={pid}"))
        );
        let original_terminal = set.lock().unwrap().pane_by_id(original).unwrap();
        // Holding the inactive terminal must not block snapshot or synthetic input.
        let held = original_terminal.lock().unwrap();
        assert!(handle(&set, &cleanup, "term.type", "").is_ok());
        assert!(handle(&set, &cleanup, "term.snapshot", "").is_ok());
        drop(held);
        assert!(handle(&set, &cleanup, "term.pane.select", "999").is_err());
        assert_eq!(
            handle(&set, &cleanup, "term.pane.close", "").unwrap(),
            "closed id=2 panes=1"
        );
        assert_eq!(
            handle(&set, &cleanup, "term.pane.select", &original.to_string()).unwrap(),
            format!("selected id={original}")
        );
        assert_eq!(
            handle(&set, &cleanup, "term.pane.close", "").unwrap(),
            format!("closed id={original} tab-closed")
        );
        assert!(set.lock().unwrap().is_empty());
        drop(cleanup);
        worker.join().unwrap();
    }
    #[test]
    fn numeric_body_only() {
        assert_eq!(parse_id("42\n"), Ok(42));
        for body in ["", "id=42", "-1", "+1", "1 2", "18446744073709551616"] {
            assert!(parse_id(body).is_err());
        }
    }
}
