use crate::tabs::{Cleanup, CompletionNote, Outcome, TabSet};
use cosmix_client::{BoundedIncomingEvent, SupervisedClient};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

pub const HELP: &str = "term: tabbed Wayland Mix terminal\nDIAGNOSTIC surface — full ABP control (windows/tabs/panes/sessions per SPEC) is P3a, gated on authenticated per-instance identity (P0-I); this self-asserted `term` name is a placeholder, not the shipped multi-user identity.\nINFO / HELP\nterm.tabs {}: list id, active, title, cols, rows, child_pid\nterm.tab.new {}: open and activate a tab\nterm.tab.select {\"id\":<integer>}: select tab\nterm.tab.close {\"id\":<integer>}: close tab; last tab quits\nThese tab verbs are DIAGNOSTIC too; real per-instance identity is P0-I.\nterm.panes {}: list active tab pane ids, focus, dimensions, child pids and logical geometry\nterm.pane.split {\"dir\":\"h|horizontal|v|vertical\"}\nterm.pane.close {}: close active pane; last pane closes tab\nterm.pane.select {\"id\":<integer>}: select pane in active tab\nThese pane verbs are DIAGNOSTIC too; real per-instance identity is P0-I.\nterm.snapshot {}: read-only active screen, dimensions, cursor, child pid, byte counters and DIAGNOSTIC timings\nterm.type {\"text\":\"<string>\"}: DIAGNOSTIC ONLY; ASCII synthetic keys to the active pane through the keyboard encoder, max 8192 bytes including JSON envelope; newline=Enter, tab, backspace, Ctrl+C/D supported. Not a product input API.\nEmpty body is {} for no-arg verbs; all term.* bodies must be JSON objects.\nDIAGNOSTIC timings are process-side, never presented-frame evidence.";
pub fn start(
    terminal: Arc<Mutex<TabSet>>,
    cleanup: Cleanup,
    mut notify_rx: tokio::sync::mpsc::UnboundedReceiver<CompletionNote>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new().name("term-bus".into()).spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("Bus runtime");
        runtime.block_on(async move {
            let result = tokio::time::timeout(Duration::from_secs(2), SupervisedClient::connect_options("term", &cosmix_config::client_helpers::resolve_noded_url()).bounded_incoming(16).connect()).await;
            let client = match result { Ok(Ok(client)) => client, _ => { eprintln!("term Bus unavailable or connection timed out"); return; } };
            let Some(mut incoming) = client.incoming_bounded() else { return; };
            // The completion-note channel is disabled (TERM_NOTIFY=0 → no sender)
            // or closes at shutdown. `recv()` on a closed channel returns `None`
            // immediately and forever, which would spin the select; the
            // precondition retires the branch on the first `None` so it is never
            // re-polled, while the loop keeps serving Bus verbs until the TabSet
            // empties.
            let mut notify_open = true;
            while !terminal.lock().unwrap().is_empty() {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {},
                    note = notify_rx.recv(), if notify_open => {
                        // Fire-and-forget "task complete" notification. interactd
                        // absent (unregistered `interact` service) is the common
                        // desktop-less case and its error is ignored, not logged
                        // per-exit.
                        match note {
                            Some(note) => notify_complete(&client, &note).await,
                            None => notify_open = false,
                        }
                    },
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

/// Emit one `interact.notify` (notify.v1) for a self-exited pane. Best-effort:
/// a 2s timeout bounds a wedged broker and every failure (interactd absent,
/// transport error, timeout) is swallowed — a missing desktop notification must
/// never disturb the terminal. `dedupe_key` is per-pane so a rapid re-exit
/// coalesces rather than stacking.
async fn notify_complete(client: &SupervisedClient, note: &CompletionNote) {
    let body = serde_json::json!({
        "summary": format!("Terminal shell exited — {}", note.tab_title),
        "body": format!("pane {} (pid {}) finished", note.pane_id, note.child_pid),
        "urgency": "normal",
        "category": "transfer.complete",
        "icon": { "lucide": "terminal" },
        "dedupe_key": format!("term-pane-{}", note.pane_id),
    });
    // `send` is fire-and-forget: it returns once the frame is dispatched, never
    // waiting on interactd, so a completion notify cannot stall the verb-serving
    // select loop. One DIAGNOSTIC line makes the otherwise-invisible dispatch
    // observable (an absent `interact` service surfaces here as a send error).
    match tokio::time::timeout(
        Duration::from_secs(2),
        client.send("interact", "interact.notify", body),
    )
    .await
    {
        Ok(Ok(())) => {
            eprintln!("DIAGNOSTIC term completion-notify dispatched: pane {}", note.pane_id)
        }
        Ok(Err(error)) => {
            eprintln!("DIAGNOSTIC term completion-notify not dispatched: {error}")
        }
        Err(_) => eprintln!("DIAGNOSTIC term completion-notify send timed out"),
    }
}

fn handle(
    set: &Mutex<TabSet>,
    cleanup: &Cleanup,
    verb: &str,
    body: &str,
) -> Result<String, String> {
    // VERIFY: every term.* verb validates its JSON contract before locking/mutation.
    let args = parse_args(verb, body)?;
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
            let id = args["id"]
                .as_u64()
                .ok_or_else(|| "internal: term verb/args desync (id)".to_string())?;
            // select wakes the event loop; refresh compares View.rendered_id
            // with active_id and uploads even without a PTY damage event.
            if tabs.select(id) {
                Ok(format!("selected id={id}"))
            } else {
                Err(format!("unknown tab id={id}"))
            }
        }
        "term.tab.close" => {
            let id = args["id"]
                .as_u64()
                .ok_or_else(|| "internal: term verb/args desync (id)".to_string())?;
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
        // VERIFY: term.pane.split handler — validated JSON direction, new active ID.
        "term.pane.split" => {
            let dir = parse_dir(
                args["dir"]
                    .as_str()
                    .ok_or_else(|| "internal: term verb/args desync (dir)".to_string())?,
            )?;
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
            let id = args["id"]
                .as_u64()
                .ok_or_else(|| "internal: term verb/args desync (id)".to_string())?;
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
                // VERIFY: term.type extracts validated text, never the JSON envelope.
                terminal
                    .listener
                    .type_text(
                        args["text"]
                            .as_str()
                            .ok_or_else(|| "internal: term verb/args desync (text)".to_string())?,
                    )
                    .map(|_| {
                        "DIAGNOSTIC synthetic keys queued; inspect input_written for actual writes"
                            .into()
                    })
            }
        }
        _ => Err("unknown verb; use HELP".into()),
    }
}

fn parse_dir(body: &str) -> Result<crate::panes::SplitDir, String> {
    match body {
        "h" | "horizontal" => Ok(crate::panes::SplitDir::Horizontal),
        "v" | "vertical" => Ok(crate::panes::SplitDir::Vertical),
        _ => Err("dir must be h|horizontal|v|vertical".into()),
    }
}

fn parse_args(verb: &str, body: &str) -> Result<serde_json::Value, String> {
    if body.len() > 8192 {
        return Err("request exceeds 8192 bytes".into());
    }
    if !verb.starts_with("term.") {
        return Ok(serde_json::json!({}));
    }
    let field = match verb {
        "term.snapshot" | "term.tabs" | "term.tab.new" | "term.panes" | "term.pane.close" => None,
        "term.type" => Some("text"),
        "term.tab.select" | "term.tab.close" | "term.pane.select" => Some("id"),
        "term.pane.split" => Some("dir"),
        _ => return Err("unknown verb; use HELP".into()),
    };
    let args: serde_json::Value = serde_json::from_str(if body.is_empty() && field.is_none() {
        "{}"
    } else {
        body
    })
    .map_err(|e| format!("body must be a JSON object: {e}"))?;
    let object = args.as_object().ok_or("body must be a JSON object")?;
    if object.keys().any(|key| Some(key.as_str()) != field) {
        return Err(format!("unexpected argument for {verb}"));
    }
    match field {
        Some("text") => {
            let text = args["text"].as_str().ok_or("text must be a string")?;
            if text.len() > 8192 {
                return Err("text exceeds 8192 bytes".into());
            }
        }
        Some("id") => {
            args["id"]
                .as_u64()
                .ok_or("id must be a non-negative integer (u64)")?;
        }
        Some("dir") => {
            parse_dir(args["dir"].as_str().ok_or("dir must be a string")?)?;
        }
        _ => {}
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pane_body_parsers() {
        assert_eq!(parse_dir("h"), Ok(crate::panes::SplitDir::Horizontal));
        assert_eq!(parse_dir("vertical"), Ok(crate::panes::SplitDir::Vertical));
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
            handle(&set, &cleanup, "term.pane.split", r#"{"dir":"v"}"#).unwrap(),
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
        assert!(handle(&set, &cleanup, "term.type", r#"{"text":""}"#).is_ok());
        assert!(handle(&set, &cleanup, "term.snapshot", "").is_ok());
        drop(held);
        assert!(handle(&set, &cleanup, "term.pane.select", r#"{"id":999}"#).is_err());
        assert_eq!(
            handle(&set, &cleanup, "term.pane.close", "").unwrap(),
            "closed id=2 panes=1"
        );
        assert_eq!(
            handle(
                &set,
                &cleanup,
                "term.pane.select",
                &format!(r#"{{"id":{original}}}"#)
            )
            .unwrap(),
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
    fn json_contracts() {
        for verb in [
            "term.snapshot",
            "term.tabs",
            "term.tab.new",
            "term.panes",
            "term.pane.close",
        ] {
            assert!(parse_args(verb, "").is_ok());
            assert!(parse_args(verb, "{}").is_ok());
            for body in ["null", "[]", "42", "raw", " ", r#"{"id":1}"#] {
                assert!(parse_args(verb, body).is_err(), "{verb}: {body}");
            }
        }
        for verb in ["term.tab.select", "term.tab.close", "term.pane.select"] {
            assert_eq!(parse_args(verb, r#"{"id":42}"#).unwrap()["id"], 42);
            assert!(parse_args(verb, r#"{"id":18446744073709551615}"#).is_ok());
            for body in [
                "",
                "42",
                "{}",
                r#"{"id":-1}"#,
                r#"{"id":1.0}"#,
                r#"{"id":"42"}"#,
                r#"{"id":18446744073709551616}"#,
            ] {
                assert!(parse_args(verb, body).is_err(), "{verb}: {body}");
            }
        }
        for dir in ["h", "v", "horizontal", "vertical"] {
            assert!(
                parse_args(
                    "term.pane.split",
                    &serde_json::json!({"dir":dir}).to_string()
                )
                .is_ok()
            );
        }
        for body in ["", "v", "{}", r#"{"dir":null}"#, r#"{"dir":"sideways"}"#] {
            assert!(parse_args("term.pane.split", body).is_err());
        }
        let text = "echo hello\n\t\u{3}";
        assert_eq!(
            parse_args("term.type", &serde_json::json!({"text":text}).to_string()).unwrap()["text"],
            text
        );
        for body in ["", "raw", "{}", r#"{"text":42}"#, r#"{"text":null}"#] {
            assert!(parse_args("term.type", body).is_err());
        }
        let boundary = serde_json::json!({"text":"a".repeat(8181)}).to_string();
        assert_eq!(boundary.len(), 8192);
        assert!(parse_args("term.type", &boundary).is_ok());
        assert!(
            parse_args(
                "term.type",
                &serde_json::json!({"text":"a".repeat(8193)}).to_string()
            )
            .is_err()
        );
        assert!(parse_args("term.snapshot", &" ".repeat(8193)).is_err());
    }
}
