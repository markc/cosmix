use crate::tabs::{Cleanup, CompletionNote, Outcome, TabSet};
use cosmix_client::{BoundedIncomingEvent, SupervisedClient};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

pub const HELP: &str = "term: tabbed Wayland Mix terminal\nMesh-open surface (2026-09-15 law): under the default posture (COSMIX_MESH_OPEN unset or != \"0\") this global name serves every verb below to any mesh or local caller, no grant required. Verbs are TARGETLESS — they act on the active tab/pane of the instance holding this name at delivery time; target-bound control (instance/incarnation/pane_generation) stays on the allocated native-session route. COSMIX_MESH_OPEN=0 restores the strict diagnostic-only lane (INFO/HELP; everything else FORBIDDEN).\nINFO / HELP\nterm.tabs {}: list id, active, title, cols, rows, child_pid\nterm.tab.new {}: open and activate a tab\nterm.tab.select {\"id\":<integer>}: select tab\nterm.tab.close {\"id\":<integer>}: close tab; last tab quits\nterm.panes {}: list active tab pane ids, focus, dimensions, child pids and logical geometry\nterm.pane.split {\"dir\":\"h|horizontal|v|vertical\"}\nterm.pane.close {}: close active pane; last pane closes tab\nterm.pane.select {\"id\":<integer>}: select pane in active tab\nterm.snapshot {}: read-only active screen, dimensions, cursor, child pid, byte counters and DIAGNOSTIC timings\nterm.type {\"text\":\"<string>\"}: ASCII synthetic keys to the active pane through the keyboard encoder, max 8192 bytes including JSON envelope; newline=Enter, tab, backspace, Ctrl+C/D supported; revokes any delegated control writer like real keys.\nEmpty body is {} for no-arg verbs; all term.* bodies must be JSON objects.\nAny MUTATING verb's body (tab.*, pane.*, type) may add \"request_id\":\"<string>\": a resend of the same request (same verb and arguments, key order free) replays the recorded reply instead of re-executing (last 128 remembered) — use it on every mutation you might resend. A reused id with a different verb or arguments is refused as a conflict. The replay is the recorded outcome of the ORIGINAL attempt; retrying after changing state (e.g. after freeing the tab limit) needs a fresh id. Reads never consult the cache and always answer current state.\nDIAGNOSTIC timings are process-side, never presented-frame evidence.";
pub fn start(
    terminal: Arc<Mutex<TabSet>>,
    cleanup: Cleanup,
    notify_rx: tokio::sync::mpsc::UnboundedReceiver<CompletionNote>,
) -> std::thread::JoinHandle<()> {
    start_at(
        terminal,
        cleanup,
        notify_rx,
        cosmix_config::client_helpers::resolve_noded_url(),
    )
}

pub(crate) fn start_at(
    terminal: Arc<Mutex<TabSet>>,
    cleanup: Cleanup,
    mut notify_rx: tokio::sync::mpsc::UnboundedReceiver<CompletionNote>,
    url: String,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new().name("term-bus".into()).spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("Bus runtime");
        runtime.block_on(async move {
            let result = tokio::time::timeout(Duration::from_secs(2), SupervisedClient::connect_options("term", &url).bounded_incoming(16).connect()).await;
            let client = match result { Ok(Ok(client)) => Arc::new(client), _ => { eprintln!("term Bus unavailable or connection timed out"); return; } };
            let Some(mut incoming) = client.incoming_bounded() else { return; };
            // The completion-note channel is disabled (TERM_NOTIFY=0 → no sender)
            // or closes at shutdown. `recv()` on a closed channel returns `None`
            // immediately and forever, which would spin the select; the
            // precondition retires the branch on the first `None` so it is never
            // re-polled, while the loop keeps serving Bus verbs until the TabSet
            // empties.
            let mut notify_open = true;
            let mut notifications = tokio::task::JoinSet::new();
            let mut replies = ReplyCache::default();
            while !terminal.lock().unwrap().is_empty() {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {},
                    note = notify_rx.recv(), if notify_open => {
                        // Sink writes can block under backpressure. Poll them in
                        // separate tracked tasks so verbs remain serviceable.
                        match note {
                            Some(note) => {
                                let client = client.clone();
                                notifications.spawn(async move { notify_complete(&client, &note).await });
                            },
                            None => notify_open = false,
                        }
                    },
                    _ = notifications.join_next(), if !notifications.is_empty() => {},
                    event = incoming.recv() => {
                        let command = match event {
                            Some(BoundedIncomingEvent::Command(c)) => c,
                            Some(BoundedIncomingEvent::Overflow { .. }) => { eprintln!("term Bus incoming overflow"); continue; },
                            None => break,
                        };
                        let result = dispatch(
                            crate::control::mesh_open(),
                            &terminal,
                            &cleanup,
                            &mut replies,
                            &command.command,
                            &command.body,
                        );
                        let (rc,body) = match result { Ok(body) => (0,body), Err(error) => (10,error) };
                        let _ = tokio::time::timeout(Duration::from_secs(2),client.respond(&command,rc,&body)).await;
                    }
                }
            }
            // The final reap can queue notes just after the TabSet becomes
            // empty. Wait for channel closure and outstanding sends together,
            // under one total deadline (not two seconds per pane).
            drain_notifications(&mut notify_rx, &mut notifications, |note| {
                let client = client.clone();
                async move { notify_complete(&client, &note).await }
            }, Duration::from_secs(2)).await;
            let _ = tokio::time::timeout(Duration::from_secs(2), client.close()).await;
        });
    }).expect("Bus thread")
}

async fn drain_notifications<F, Fut>(
    receiver: &mut tokio::sync::mpsc::UnboundedReceiver<CompletionNote>,
    tasks: &mut tokio::task::JoinSet<()>,
    send: F,
    budget: Duration,
) where
    F: Fn(CompletionNote) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let _ = tokio::time::timeout(budget, async {
        let mut open = true;
        while open || !tasks.is_empty() {
            tokio::select! {
                note = receiver.recv(), if open => match note {
                    Some(note) => { tasks.spawn(send(note)); },
                    None => open = false,
                },
                _ = tasks.join_next(), if !tasks.is_empty() => {},
            }
        }
    })
    .await;
    tasks.abort_all();
}

/// Emit one `interact.notify` (notify.v1) for a self-exited pane. Best-effort:
/// a 2s timeout bounds a wedged broker and every failure (interactd absent,
/// transport error, timeout) is swallowed — a missing desktop notification must
/// never disturb the terminal. `dedupe_key` is a stable per-pane key; pane IDs
/// are never reused, so it does not coalesce separate exits.
async fn notify_complete(client: &SupervisedClient, note: &CompletionNote) {
    let body = serde_json::json!({
        "summary": format!("Terminal shell exited — {}", note.tab_title),
        "body": format!("pane {} (pid {}) finished", note.pane_id, note.child_pid),
        "urgency": "normal",
        "category": "transfer.complete",
        "icon": { "lucide": "terminal" },
        "dedupe_key": format!("term-pane-{}", note.pane_id),
    });
    // `send` waits for the WebSocket sink write, which can stall under
    // backpressure; callers run this future in a separate task. It does not
    // wait for interactd's reply. One DIAGNOSTIC line makes dispatch
    // observable (an absent `interact` service surfaces here as a send error).
    match tokio::time::timeout(
        Duration::from_secs(2),
        client.send("interact", "interact.notify", body),
    )
    .await
    {
        Ok(Ok(())) => {
            eprintln!(
                "DIAGNOSTIC term completion-notify dispatched: pane {}",
                note.pane_id
            )
        }
        Ok(Err(error)) => {
            eprintln!("DIAGNOSTIC term completion-notify not dispatched: {error}")
        }
        Err(_) => eprintln!("DIAGNOSTIC term completion-notify send timed out"),
    }
}

/// Replayed replies for retried mutations. The lane's verbs are targetless
/// and several are non-idempotent (`term.tab.new`, `term.pane.close` takes no
/// argument at all), so a caller whose reply was lost must be able to retry
/// without double-executing: resend the byte-identical request with the same
/// `request_id` and the recorded reply is returned verbatim, the verb NOT
/// re-run. The lane has no caller identity, so the id alone cannot be the
/// key: each entry remembers its (verb, body) and a same-id request that
/// differs in either is refused as a conflict rather than silently answered
/// with another request's reply. Bounded FIFO — no clock, no TTL, eviction
/// order is insertion order.
#[derive(Default)]
struct ReplyCache {
    map: std::collections::HashMap<String, CachedReply>,
    order: std::collections::VecDeque<String>,
}
struct CachedReply {
    verb: String,
    body: serde_json::Value,
    reply: Result<String, String>,
}
const REPLY_CACHE_CAP: usize = 128;
const REQUEST_ID_CONFLICT: &str =
    "request_id already used by a different request; retries must resend the identical verb and body";
impl ReplyCache {
    /// Some(reply) = do not execute (a replay, or the conflict refusal);
    /// None = a genuinely new id, execute and record. Bodies are compared as
    /// parsed JSON, not bytes: a client that rebuilds its retry from a map
    /// may reorder keys, and that retry is the one this cache exists for.
    fn lookup(
        &self,
        id: &str,
        verb: &str,
        body: &serde_json::Value,
    ) -> Option<Result<String, String>> {
        let entry = self.map.get(id)?;
        if entry.verb == verb && entry.body == *body {
            Some(entry.reply.clone())
        } else {
            Some(Err(REQUEST_ID_CONFLICT.into()))
        }
    }
    fn put(
        &mut self,
        id: String,
        verb: &str,
        body: serde_json::Value,
        reply: Result<String, String>,
    ) {
        let entry = CachedReply {
            verb: verb.into(),
            body,
            reply,
        };
        if self.map.insert(id.clone(), entry).is_none() {
            self.order.push_back(id);
            if self.order.len() > REPLY_CACHE_CAP
                && let Some(evicted) = self.order.pop_front()
            {
                self.map.remove(&evicted);
            }
        }
    }
}

/// The verbs whose execution changes state. Only these consult or feed the
/// replay cache: reads must always answer with CURRENT state (a caller
/// templating one constant request_id into everything would otherwise see a
/// frozen terminal), and read replies (whole snapshot dumps) would also be
/// the cache's largest entries.
fn mutates(verb: &str) -> bool {
    matches!(
        verb,
        "term.tab.new"
            | "term.tab.select"
            | "term.tab.close"
            | "term.pane.split"
            | "term.pane.select"
            | "term.pane.close"
            | "term.type"
    )
}

/// Mesh-open law (2026-09-15): every Bus verb of every app is reachable by
/// any mesh/local caller with no authorization gate. Under the default-open
/// posture the global name serves the full active-tab verb set;
/// COSMIX_MESH_OPEN=0 restores the diagnostic-only lane (protected controls
/// native-session-route only). An optional `request_id` body field makes a
/// retry replay the recorded reply instead of re-executing the verb.
fn dispatch(
    open: bool,
    set: &Mutex<TabSet>,
    cleanup: &Cleanup,
    replies: &mut ReplyCache,
    verb: &str,
    body: &str,
) -> Result<String, String> {
    if !open {
        return diagnostic(verb);
    }
    // The envelope size limit applies before any parse, lookup or caching:
    // an oversized body must neither hit the replay cache nor leave its
    // request_id resident in it.
    if body.len() > 8192 {
        return Err("request exceeds 8192 bytes".into());
    }
    // The request_id is extracted from the raw body (not parse_args' output)
    // so a replay never depends on the verb's own argument validation; a
    // malformed body simply has no request_id and falls through to handle(),
    // whose validation answers as usual. Only mutating verbs touch the
    // cache — reads always execute against current state.
    let parsed = if mutates(verb) {
        serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .filter(|args| args["request_id"].is_string())
    } else {
        None
    };
    if let Some(args) = &parsed {
        let id = args["request_id"].as_str().expect("filtered as string");
        if let Some(reply) = replies.lookup(id, verb, args) {
            return reply;
        }
    }
    let result = handle(set, cleanup, verb, body);
    if let Some(args) = parsed {
        let id = args["request_id"].as_str().expect("filtered as string");
        replies.put(id.to_owned(), verb, args, result.clone());
    }
    result
}

/// The global, self-asserted TCP name is never an authority boundary.
/// Keep discovery/notification compatibility, but fail closed for all data and
/// controls, including when native bootstrap is unavailable.
fn diagnostic(verb: &str) -> Result<String, String> {
    match verb {
        "INFO" | "HELP" | "info" | "help" => Ok(
            "term: diagnostic discovery only; protected controls require the allocated native-session route".into(),
        ),
        _ => Err("{\"error_code\":\"FORBIDDEN\"}".into()),
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
        "INFO" | "HELP" | "info" | "help" => Ok(format!(
            "{HELP}\nterm.session {{}}: native identity and per-pane binding diagnostics (not live authority)"
        )),
        "term.session" => Ok(tabs.session_status().to_string()),
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
            let active = tabs.active_terminal();
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
        "term.snapshot" | "term.tabs" | "term.tab.new" | "term.panes" | "term.pane.close"
        | "term.session" => None,
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
    // `request_id` rides alongside any verb's own argument: it addresses the
    // reply-replay cache in dispatch(), never the verb itself.
    if object
        .keys()
        .any(|key| Some(key.as_str()) != field && key != "request_id")
    {
        return Err(format!("unexpected argument for {verb}"));
    }
    if object.contains_key("request_id") {
        args["request_id"]
            .as_str()
            .ok_or("request_id must be a string")?;
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
    fn diagnostic_lane_has_no_protected_controls() {
        for verb in [
            "term.session",
            "term.tabs",
            "term.panes",
            "term.snapshot",
            "term.type",
            "term.tab.new",
            "term.tab.select",
            "term.tab.close",
            "term.pane.split",
            "term.pane.select",
            "term.pane.close",
            "term.execute",
            "props.get",
            "props.set",
            "props.watch",
        ] {
            assert_eq!(
                diagnostic(verb).unwrap_err(),
                "{\"error_code\":\"FORBIDDEN\"}"
            );
        }
        assert!(diagnostic("HELP").is_ok());
    }
    #[test]
    fn notification_drain_keeps_final_reap_and_bounds_blocked_sends() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
                let delivered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let mut tasks = tokio::task::JoinSet::new();
                // Simulate the last reap queuing notes after the verb loop ends.
                tokio::spawn(async move {
                    tokio::task::yield_now().await;
                    for pane_id in 1..=3 {
                        tx.send(CompletionNote {
                            pane_id,
                            tab_title: "test".into(),
                            child_pid: 1,
                        })
                        .unwrap();
                    }
                });
                drain_notifications(
                    &mut rx,
                    &mut tasks,
                    |_| {
                        let delivered = delivered.clone();
                        async move {
                            delivered.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        }
                    },
                    Duration::from_secs(1),
                )
                .await;
                assert_eq!(delivered.load(std::sync::atomic::Ordering::SeqCst), 3);
                assert!(tasks.is_empty());

                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
                for pane_id in 1..=3 {
                    tx.send(CompletionNote {
                        pane_id,
                        tab_title: "test".into(),
                        child_pid: 1,
                    })
                    .unwrap();
                }
                // A backpressured send stays pending while another task makes
                // progress; an open producer must not prevent the total deadline.
                let started = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                tokio::time::timeout(
                    Duration::from_secs(1),
                    drain_notifications(
                        &mut rx,
                        &mut tasks,
                        |_| {
                            let started = started.clone();
                            async move {
                                started.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                std::future::pending::<()>().await;
                            }
                        },
                        Duration::from_millis(30),
                    ),
                )
                .await
                .expect("shared drain deadline");
                assert_eq!(started.load(std::sync::atomic::Ordering::SeqCst), 3);
                while let Some(result) = tasks.join_next().await {
                    assert!(result.unwrap_err().is_cancelled());
                }
                drop(tx);
            });
    }

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
    fn dispatch_posture_gate_and_request_id_replay() {
        if !std::path::Path::new("/opt/cosmix/bin/mix").is_file() {
            eprintln!("SKIP dispatch Bus test: Mix unavailable");
            return;
        }
        let set = Mutex::new(TabSet::new().unwrap());
        let (cleanup, worker) = Cleanup::start().unwrap();
        let mut replies = ReplyCache::default();
        // Strict posture: diagnostic-only, protected verbs refused.
        assert_eq!(
            dispatch(false, &set, &cleanup, &mut replies, "term.tabs", "").unwrap_err(),
            "{\"error_code\":\"FORBIDDEN\"}"
        );
        assert!(dispatch(false, &set, &cleanup, &mut replies, "HELP", "").is_ok());
        // Open posture: the full verb set answers on the global name.
        assert!(dispatch(true, &set, &cleanup, &mut replies, "term.tabs", "").is_ok());
        // A retried mutation with the same request_id replays the recorded
        // reply and must NOT re-execute: still two panes after the retry.
        let body = r#"{"dir":"v","request_id":"r1"}"#;
        let first = dispatch(true, &set, &cleanup, &mut replies, "term.pane.split", body).unwrap();
        let replay = dispatch(true, &set, &cleanup, &mut replies, "term.pane.split", body).unwrap();
        assert_eq!(first, replay);
        assert_eq!(
            dispatch(true, &set, &cleanup, &mut replies, "term.panes", "")
                .unwrap()
                .lines()
                .count(),
            2
        );
        // A different request_id executes again.
        let second = dispatch(
            true,
            &set,
            &cleanup,
            &mut replies,
            "term.pane.split",
            r#"{"dir":"v","request_id":"r2"}"#,
        )
        .unwrap();
        assert_ne!(first, second);
        assert_eq!(
            dispatch(true, &set, &cleanup, &mut replies, "term.panes", "")
                .unwrap()
                .lines()
                .count(),
            3
        );
        // Error replies replay too — the failed attempt is not retried.
        let bad = r#"{"id":999,"request_id":"r3"}"#;
        let refusal =
            dispatch(true, &set, &cleanup, &mut replies, "term.pane.select", bad).unwrap_err();
        assert_eq!(
            dispatch(true, &set, &cleanup, &mut replies, "term.pane.select", bad).unwrap_err(),
            refusal
        );
        // A reused id with a DIFFERENT verb or body is a conflict, never a
        // silent replay of the other request's reply — and never executes:
        // still three panes afterwards.
        assert_eq!(
            dispatch(
                true,
                &set,
                &cleanup,
                &mut replies,
                "term.pane.split",
                r#"{"dir":"h","request_id":"r1"}"#,
            )
            .unwrap_err(),
            REQUEST_ID_CONFLICT
        );
        assert_eq!(
            dispatch(
                true,
                &set,
                &cleanup,
                &mut replies,
                "term.tab.new",
                r#"{"request_id":"r1"}"#,
            )
            .unwrap_err(),
            REQUEST_ID_CONFLICT
        );
        assert_eq!(
            dispatch(true, &set, &cleanup, &mut replies, "term.panes", "")
                .unwrap()
                .lines()
                .count(),
            3
        );
        // A resend with reordered keys is the same request: replayed, not
        // conflicted, not re-executed (still three panes).
        assert_eq!(
            dispatch(
                true,
                &set,
                &cleanup,
                &mut replies,
                "term.pane.split",
                r#"{"request_id":"r1","dir":"v"}"#,
            )
            .unwrap(),
            first
        );
        // Reads never consult the cache: a reused id answers current state.
        assert_eq!(
            dispatch(
                true,
                &set,
                &cleanup,
                &mut replies,
                "term.panes",
                r#"{"request_id":"r1"}"#,
            )
            .unwrap()
            .lines()
            .count(),
            3
        );
        // An oversized body is refused before the cache sees it: its id stays
        // unused, so the same id afterwards executes normally.
        let oversized = format!(
            r#"{{"text":"{}","request_id":"r4"}}"#,
            "a".repeat(8192)
        );
        assert_eq!(
            dispatch(true, &set, &cleanup, &mut replies, "term.type", &oversized).unwrap_err(),
            "request exceeds 8192 bytes"
        );
        assert!(
            dispatch(
                true,
                &set,
                &cleanup,
                &mut replies,
                "term.type",
                r#"{"text":"","request_id":"r4"}"#,
            )
            .is_ok()
        );
        set.lock().unwrap().shutdown();
        drop(cleanup);
        worker.join().unwrap();
    }
    #[test]
    fn reply_cache_is_bounded_fifo() {
        let body = serde_json::json!({"dir":"v","request_id":"x"});
        let mut replies = ReplyCache::default();
        for n in 0..=REPLY_CACHE_CAP {
            replies.put(format!("id-{n}"), "v", body.clone(), Ok(format!("reply-{n}")));
        }
        assert_eq!(replies.lookup("id-0", "v", &body), None);
        assert_eq!(
            replies.lookup("id-1", "v", &body),
            Some(Ok("reply-1".into()))
        );
        assert_eq!(
            replies.lookup(&format!("id-{REPLY_CACHE_CAP}"), "v", &body),
            Some(Ok(format!("reply-{REPLY_CACHE_CAP}")))
        );
        // Bodies compare as parsed JSON — key order must not matter.
        let reordered = serde_json::from_str(r#"{"request_id":"x","dir":"v"}"#).unwrap();
        assert_eq!(
            replies.lookup("id-1", "v", &reordered),
            Some(Ok("reply-1".into()))
        );
        // A same-id lookup with a different verb or body is a conflict, not
        // a replay and not a miss.
        assert_eq!(
            replies.lookup("id-1", "other", &body),
            Some(Err(REQUEST_ID_CONFLICT.into()))
        );
        assert_eq!(
            replies.lookup("id-1", "v", &serde_json::json!({"dir":"h","request_id":"x"})),
            Some(Err(REQUEST_ID_CONFLICT.into()))
        );
        // Overwriting an existing id must not grow the eviction queue.
        replies.put("id-1".into(), "v", body, Ok("changed".into()));
        assert_eq!(replies.map.len(), replies.order.len());
    }
    #[test]
    fn json_contracts() {
        for verb in [
            "term.session",
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
        // request_id rides alongside any verb's own argument; wrong type refused.
        assert!(parse_args("term.tab.new", r#"{"request_id":"r1"}"#).is_ok());
        assert!(parse_args("term.pane.split", r#"{"dir":"v","request_id":"r1"}"#).is_ok());
        assert!(parse_args("term.tab.select", r#"{"id":1,"request_id":"r1"}"#).is_ok());
        assert!(parse_args("term.tab.new", r#"{"request_id":42}"#).is_err());
        assert!(parse_args("term.tab.new", r#"{"request_id":null}"#).is_err());
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
