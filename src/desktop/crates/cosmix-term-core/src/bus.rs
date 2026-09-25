use crate::tabs::{Change, Cleanup, CompletionNote, Outcome, TabSet};
use cosmix_client::{BoundedIncomingEvent, IncomingCommand, SupervisedClient};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

pub const HELP: &str = "term: tabbed Wayland Mix terminal\nMesh-open surface (2026-09-15 law): under the default posture (COSMIX_MESH_OPEN unset or != \"0\") this global name serves every verb below to any mesh or local caller, no grant required. Verbs are TARGETLESS — unless explicit pane/tab selectors are supplied, they act on the active tab/pane of the instance holding this name at delivery time; target-bound control (instance/incarnation/pane_generation) stays on the allocated native-session route. COSMIX_MESH_OPEN=0 restores the strict diagnostic-only lane (INFO/HELP; everything else FORBIDDEN).\nINFO / HELP\nterm.tabs {}: list id, active, title, cols, rows, child_pid\nterm.tab.new {cwd?:<absolute existing directory>, title?:<string>}: open and activate a tab; the reply adds binding=granted (native launch grant delivered, enrolment async), graphics-only (no usable grant) or unavailable (no native session)\nterm.tab.select {\"id\":<integer>}: select tab\nterm.tab.close {\"id\":<integer>}: close tab; last tab quits\nterm.panes {tab?:<integer>}: list selected tab (default active tab) pane ids, focus, dimensions, child pids and logical geometry (last layout only; hidden tabs may be stale or zero)\nterm.pane.split {\"dir\":\"h|horizontal|v|vertical\"}\nterm.pane.close {}: close active pane; last pane closes tab\nterm.pane.select {\"id\":<integer>}: select pane in active tab\nterm.snapshot {pane?:<integer>, tab?:<integer>, contents?:<boolean=true>, scrollback_lines?:<integer=0, max 10000>}: read-only selected live screen (offset zero, default active); buffered history above the live screen is capped at available lines; text has a 512 KiB encoded-byte budget, returns complete oldest-first rows with truncated and lines_returned; formatting happens after releasing capture locks; pane+tab must agree; contents=false omits text but keeps dimensions, cursor, child pid, byte counters and DIAGNOSTIC timings\nterm.scroll {pane?:<integer>, lines?:<signed integer>, page?:<signed integer>, to?:top|bottom}: exactly one of lines/page/to; positive lines or pages move up into history; pages overlap by one row; viewport only, snapshots stay live; returns JSON {pane,display_offset,history_lines}; stale pane is not-found; changed offsets publish pane.changed kind=scrolled when watching\nterm.type {pane?:<integer>, \"text\":\"<string>\"}: ASCII synthetic keys to the selected pane without changing focus (default active pane) through the keyboard encoder, max 8192 bytes including JSON envelope; newline=Enter, tab, backspace, Ctrl+C/D supported; revokes any delegated control writer like real keys.\nterm.tab.title {id:<integer>, title:<string>}: pin a user title (controls stripped, max 256 UTF-8 bytes); empty after sanitising clears the pin and restores the active pane OSC title\nterm.tab.move {id:<integer>, index:<integer>}: reorder tab, clamping index to 0..len-1; focus is preserved\nterm.props.watch {}: first subscribe through noded topic.subscribe to term.tabs.changed, term.pane.changed and term.title.changed; then call this verb to enable caller-free publishing and return JSON {topics,revision}; then read current state. Bodies are {tab,pane,kind,revision}; revision is a separate monotonic event sequence, not the legacy layout revision. Bounded best-effort delivery; on a gap or reconnect read current state.\nEmpty body is {} for no-arg verbs; all term.* bodies must be JSON objects.\nAny MUTATING verb's body (tab.*, pane.*, type, scroll) may add \"request_id\":\"<string>\": a resend of the same request (same verb and arguments, key order free) replays the recorded reply instead of re-executing (last 128 remembered) — use it on every mutation you might resend. A reused id with a different verb or arguments is refused as a conflict. The replay is the recorded outcome of the ORIGINAL attempt; retrying after changing state (e.g. after freeing the tab limit) needs a fresh id. Reads never consult the cache and always answer current state.\nReplies echo the identity acted on as key=value tokens — tab=<id> pane=<id> revision=<tab-set revision> (tab.close: revision only; pane.close: tab and revision; list lines: revision, panes also tab) — so a caller can detect drift after the fact; it is detection, not binding.\nDIAGNOSTIC timings are process-side, never presented-frame evidence.";
/// The one spelling the handlers in this crate are written in.
///
/// D1 (TODO-term, 2026-09-21): two binaries cannot both own the global Bus
/// name `term`, and T5's A/B weight comparison requires both frontends
/// running at once — so the Bevy frontend registers as `bterm` and serves
/// `bterm.*`, the incoming iced one as `term` / `term.*`. **Nothing in this
/// crate may hardcode either name**: the frontend passes its own in, and the
/// wire namespace follows it.
///
/// The 147 handler arms keep this single canonical spelling instead of being
/// rewritten to build verb strings; [`canonical_verb`] rewrites an incoming
/// `<service>.` prefix to it at the dispatch boundary. So the wire name is a
/// parameter while the handlers stay one implementation, which is also what
/// keeps the two frontends behaviourally identical rather than merely
/// similar.
pub const CANONICAL: &str = "term";

/// Rewrite a verb from the wire namespace (`<service>.foo`) into the
/// canonical one (`term.foo`), or `None` when it belongs to some other
/// namespace and this frontend must not answer it.
///
/// Unprefixed verbs — `INFO`, `HELP` — pass through: they are discovery, not
/// a namespace, and both frontends answer them.
///
/// **The `None` arm is the whole point of D1, and it was missing on the first
/// cut.** Rewriting only `<service>.` while letting a bare `term.` fall
/// through left bterm answering BOTH namespaces, which is the collision the
/// rename exists to prevent: a mesh caller sending `term.tab.new` would be
/// served by whichever frontend happened to hold the name, and T5's A/B would
/// be comparing one terminal wearing two hats.
/// `bterm_serves_its_own_namespace_and_refuses_terms` is the gate, and it
/// failed before this arm existed.
fn canonical_verb<'a>(service: &str, verb: &'a str) -> Option<std::borrow::Cow<'a, str>> {
    if let Some(rest) = verb.strip_prefix(service).and_then(|r| r.strip_prefix('.')) {
        return Some(if service == CANONICAL {
            std::borrow::Cow::Borrowed(verb)
        } else {
            std::borrow::Cow::Owned(format!("{CANONICAL}.{rest}"))
        });
    }
    // A dot means the caller aimed at a namespace, and it is not ours.
    match verb.contains('.') {
        true => None,
        false => Some(std::borrow::Cow::Borrowed(verb)),
    }
}

/// [`HELP`] rendered into `service`'s namespace, so a caller reads the verb
/// names it can actually send. `help_renames_every_verb` pins that the
/// rewrite is total — a future HELP edit that spells a verb some other way
/// fails that test rather than advertising an unroutable name.
pub fn help(service: &str) -> String {
    if service == CANONICAL {
        return HELP.to_string();
    }
    HELP.replace(&format!("{CANONICAL}."), &format!("{service}."))
        .replace(&format!("{CANONICAL}:"), &format!("{service}:"))
}

pub fn start(
    service: &'static str,
    terminal: Arc<Mutex<TabSet>>,
    cleanup: Cleanup,
    notify_rx: tokio::sync::mpsc::UnboundedReceiver<CompletionNote>,
) -> std::thread::JoinHandle<()> {
    start_at(
        service,
        terminal,
        cleanup,
        notify_rx,
        cosmix_config::client_helpers::resolve_noded_url(),
    )
}

pub(crate) fn start_at(
    service: &'static str,
    terminal: Arc<Mutex<TabSet>>,
    cleanup: Cleanup,
    mut notify_rx: tokio::sync::mpsc::UnboundedReceiver<CompletionNote>,
    url: String,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new().name(format!("{service}-bus")).spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("Bus runtime");
        runtime.block_on(async move {
            let result = tokio::time::timeout(Duration::from_secs(2), SupervisedClient::connect_options(service, &url).bounded_incoming(16).connect()).await;
            let client = match result { Ok(Ok(client)) => Arc::new(client), _ => { eprintln!("{service} Bus unavailable or connection timed out"); return; } };
            let Some(mut incoming) = client.incoming_bounded() else { return; };
            let mut notifications = tokio::task::JoinSet::new();
            serve(service, &terminal, &cleanup, &mut notify_rx, &mut notifications, &mut incoming, &client).await;
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

/// Where the serving loop's commands come from. The broker's bounded lane in
/// production; a plain channel under test, so the loop itself runs without a
/// broker.
trait Incoming {
    fn next(&mut self) -> impl std::future::Future<Output = Option<BoundedIncomingEvent>>;
}
impl Incoming for cosmix_client::BoundedIncomingReceiver {
    fn next(&mut self) -> impl std::future::Future<Output = Option<BoundedIncomingEvent>> {
        self.recv()
    }
}

/// Where the serving loop's replies and completion notes go.
trait Peer {
    fn reply(
        &self,
        command: &IncomingCommand,
        rc: u8,
        body: &str,
    ) -> impl std::future::Future<Output = ()>;
    fn completed(
        &self,
        note: CompletionNote,
    ) -> impl std::future::Future<Output = ()> + Send + 'static;
    fn changed(
        &self,
        service: &str,
        change: Change,
    ) -> impl std::future::Future<Output = ()> + Send + 'static;
}
impl Peer for Arc<SupervisedClient> {
    fn changed(
        &self,
        service: &str,
        change: Change,
    ) -> impl std::future::Future<Output = ()> + Send + 'static {
        let client = self.clone();
        let topic = format!("{service}.{}", change.topic);
        async move {
            let headers = std::collections::BTreeMap::from([
                ("name".into(), topic),
                ("retain".into(), "false".into()),
            ]);
            let mut message = cosmix_bus::bus::BusMessage::new();
            message.set("command", change.topic);
            message.body = change.body().to_string();
            let wire = message.to_wire();
            let result = tokio::time::timeout(
                Duration::from_secs(2),
                client.call_with_headers_raw("noded", "topic.publish", &headers, &wire),
            )
            .await;
            if !matches!(result, Ok(Ok((0..=9, _, _)))) {
                static LAST_FAILURE: Mutex<Option<std::time::Instant>> = Mutex::new(None);
                let mut last = LAST_FAILURE.lock().unwrap();
                if last.is_none_or(|at| at.elapsed() >= Duration::from_secs(30)) {
                    eprintln!(
                        "terminal change topic publication failed at revision {} (logging at most once per 30s; read state to resynchronise)",
                        change.revision
                    );
                    *last = Some(std::time::Instant::now());
                }
            }
        }
    }
    async fn reply(&self, command: &IncomingCommand, rc: u8, body: &str) {
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            SupervisedClient::respond(self, command, rc, body),
        )
        .await;
    }
    fn completed(
        &self,
        note: CompletionNote,
    ) -> impl std::future::Future<Output = ()> + Send + 'static {
        let client = self.clone();
        async move { notify_complete(&client, &note).await }
    }
}

/// Serve verbs until the TabSet empties. Event-driven throughout: every
/// branch is a real event (a command, a completion note, a finished send, the
/// last tab closing), so an idle terminal never runs a turn — the loop used
/// to tick a 100 ms sleep purely to re-check `is_empty()`. Returns the number
/// of turns taken, which is what the idle test counts.
async fn serve(
    service: &str,
    terminal: &Mutex<TabSet>,
    cleanup: &Cleanup,
    notify_rx: &mut tokio::sync::mpsc::UnboundedReceiver<CompletionNote>,
    notifications: &mut tokio::task::JoinSet<()>,
    incoming: &mut impl Incoming,
    peer: &impl Peer,
) -> usize {
    // Taken once: the TabSet signals this whichever thread closes the last
    // tab — a Bus verb here, or the frontend (keyboard, child exit, window
    // close) — and the permit survives until this loop next waits.
    let emptied = terminal.lock().unwrap().emptied();
    let titles_changed = terminal.lock().unwrap().titles_changed();
    let mut changes = terminal.lock().unwrap().observe();
    let mut publishing = tokio::task::JoinSet::new();
    // The completion-note channel is disabled (TERM_NOTIFY=0 → no sender)
    // or closes at shutdown. `recv()` on a closed channel returns `None`
    // immediately and forever, which would spin the select; the
    // precondition retires the branch on the first `None` so it is never
    // re-polled, while the loop keeps serving Bus verbs until the TabSet
    // empties.
    let mut notify_open = true;
    let mut replies = ReplyCache::default();
    let mut turns = 0;
    while !terminal.lock().unwrap().is_empty() {
        turns += 1;
        tokio::select! {
            _ = emptied.notified() => {},
            _ = titles_changed.notified(), if terminal.lock().unwrap().is_watching() => {
                terminal.lock().unwrap().refresh_titles();
            },
            Some(change) = changes.recv(), if publishing.is_empty() => {
                publishing.spawn(peer.changed(service, change));
            },
            _ = publishing.join_next(), if !publishing.is_empty() => {},
            note = notify_rx.recv(), if notify_open => {
                // Sink writes can block under backpressure. Poll them in
                // separate tracked tasks so verbs remain serviceable.
                match note {
                    Some(note) => { notifications.spawn(peer.completed(note)); },
                    None => notify_open = false,
                }
            },
            _ = notifications.join_next(), if !notifications.is_empty() => {},
            event = incoming.next() => {
                let command = match event {
                    Some(BoundedIncomingEvent::Command(c)) => c,
                    Some(BoundedIncomingEvent::Overflow { .. }) => { eprintln!("{service} Bus incoming overflow"); continue; },
                    None => break,
                };
                let guarded = guard(terminal, std::panic::AssertUnwindSafe(|| dispatch(
                    crate::control::mesh_open(),
                    service,
                    terminal,
                    cleanup,
                    &mut replies,
                    &command.command,
                    &command.body,
                )));
                let Ok(result) = guarded else {
                    // The panic unwound through a held TabSet lock: the set may
                    // be half-mutated, and the frontend's next lock would panic
                    // on the poison anyway, somewhere less legible. Stop here,
                    // saying why, rather than serve verbs over torn state.
                    eprintln!("{service} Bus verb {:?} panicked while holding the tab set; aborting", command.command);
                    std::process::abort();
                };
                let (rc, body) = match result { Ok(body) => (0, body), Err(error) => (10, error) };
                peer.reply(&command, rc, &body).await;
            }
        }
    }
    // Include final removal records. One total shutdown budget bounds a
    // stalled broker; publication stays serial to preserve event ordering.
    changes.close();
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        while publishing.join_next().await.is_some() {}
        while let Some(change) = changes.recv().await {
            peer.changed(service, change).await;
        }
    })
    .await;
    publishing.abort_all();
    turns
}

const HANDLER_PANICKED: &str = "internal error: verb handler panicked";

/// A panic that unwound through the tab-set lock, poisoning it.
#[derive(Debug, PartialEq, Eq)]
struct Torn;

/// Panic boundary for one Bus verb. Before this, a handler panic killed the
/// Bus thread silently (the frontend discards its join result) while every
/// other verb went unanswered.
///
/// The TabSet lock decides the outcome. Terminal, native-session and control
/// state have locks of their own, but every one a handler takes is taken
/// while it holds the TabSet lock, so a panic escaping any of them unwinds
/// through the set lock and poisons it too. A poisoned set may be
/// half-mutated: that is [`Torn`], and the caller must not carry on (serve()
/// aborts before replying, so the in-flight caller gets no reply). An
/// unpoisoned set means the panic happened outside those nested critical
/// sections; the caller gets an ordinary error and the lane keeps serving.
///
/// Two limits. The boundary covers this one verb on this thread: panics on
/// other threads, or at serve()'s own unguarded `is_empty()` lock, are
/// outside it. And "unpoisoned" is not quite "nothing changed": `tab.close`
/// and `pane.close` release the set lock before `cleanup.submit`, so a panic
/// there would follow a completed close — and since the reply cache is only
/// written after `handle()` returns, a retried targetless `pane.close` would
/// close a DIFFERENT pane. Theoretical today (`submit` is a `let _ = send`),
/// but anything added after those `drop(tabs)` calls inherits it.
fn guard<T>(
    set: &Mutex<T>,
    run: impl FnOnce() -> Result<String, String> + std::panic::UnwindSafe,
) -> Result<Result<String, String>, Torn> {
    match std::panic::catch_unwind(run) {
        Ok(result) => Ok(result),
        Err(_) if set.is_poisoned() => Err(Torn),
        Err(_) => Ok(Err(HANDLER_PANICKED.into())),
    }
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
            | "term.tab.title"
            | "term.tab.move"
            | "term.tab.select"
            | "term.tab.close"
            | "term.pane.split"
            | "term.pane.select"
            | "term.pane.close"
            | "term.type"
            | "term.scroll"
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
    service: &str,
    set: &Mutex<TabSet>,
    cleanup: &Cleanup,
    replies: &mut ReplyCache,
    wire_verb: &str,
    body: &str,
) -> Result<String, String> {
    // Into the canonical namespace once, at the boundary, and never back out:
    // the replay cache, the mutation gate, argument validation and every
    // handler below all see `term.*` whatever name this frontend serves
    // under. Doing it here rather than per-handler is what keeps `bterm` and
    // `term` the same implementation instead of two that drift (D1).
    // Posture gate first, and on the WIRE verb, so the strict lane keeps its
    // stated contract exactly: INFO/HELP answer, everything else is
    // FORBIDDEN — including a foreign namespace, which must not be able to
    // tell itself apart from a refused one.
    if !open {
        return diagnostic(service, wire_verb);
    }
    let Some(verb) = canonical_verb(service, wire_verb) else {
        // Another frontend's namespace. Refused with the same message an
        // unknown verb in our OWN namespace gets, so the reply is not an
        // oracle for which other frontends exist.
        return Err("unknown verb; use HELP".into());
    };
    let verb = verb.as_ref();
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
    let result = handle(service, set, cleanup, verb, body);
    if let Some(args) = parsed {
        let id = args["request_id"].as_str().expect("filtered as string");
        replies.put(id.to_owned(), verb, args, result.clone());
    }
    result
}

/// The global, self-asserted TCP name is never an authority boundary.
/// Keep discovery/notification compatibility, but fail closed for all data and
/// controls, including when native bootstrap is unavailable.
fn diagnostic(service: &str, verb: &str) -> Result<String, String> {
    match verb {
        "INFO" | "HELP" | "info" | "help" => Ok(format!(
            "{service}: diagnostic discovery only (posture=strict, COSMIX_MESH_OPEN=0); protected controls require the allocated native-session route"
        )),
        _ => Err("{\"error_code\":\"FORBIDDEN\"}".into()),
    }
}

fn handle(
    service: &str,
    set: &Mutex<TabSet>,
    cleanup: &Cleanup,
    verb: &str,
    body: &str,
) -> Result<String, String> {
    // VERIFY: every term.* verb validates its JSON contract before locking/mutation.
    let args = parse_args(verb, body)?;
    // Test-only verbs that drive the panic boundary through the real
    // serve() path: one panics before the set lock, one while holding it.
    #[cfg(test)]
    if verb == "term.test.panic" {
        panic!("test verb: panic outside the tab-set lock");
    }
    let mut tabs = set.lock().unwrap();
    tabs.refresh_titles();
    #[cfg(test)]
    if verb == "term.test.panic_locked" {
        panic!("test verb: panic holding the tab-set lock");
    }
    match verb {
        "INFO" | "HELP" | "info" | "help" => Ok(format!(
            "{}\n{service}.session {{}}: native identity and per-pane binding diagnostics (not live authority)",
            help(service)
        )),
        "term.session" => {
            let mut status = tabs.session_status();
            // dispatch() refuses every verb but INFO/HELP under the strict
            // posture before reaching here, so a handler reply is mesh-open.
            status["posture"] = crate::control::posture(true).into();
            Ok(status.to_string())
        }
        "term.tabs" => Ok(tabs
            .list()
            .iter()
            .map(|tab| {
                format!(
                    "id={} active={} title={} cols={} rows={} child_pid={} revision={}",
                    tab.id, tab.active, tab.title, tab.cols, tab.rows, tab.child_pid, tabs.revision
                )
            })
            .collect::<Vec<_>>()
            .join("\n")),
        "term.props.watch" => {
            let revision = tabs.watch();
            Ok(
                serde_json::json!({"topics": [format!("{service}.tabs.changed"),
                format!("{service}.pane.changed"), format!("{service}.title.changed")],
                "revision": revision})
                .to_string(),
            )
        }
        "term.tab.title" => {
            let id = args["id"].as_u64().unwrap();
            tabs.set_title(id, args["title"].as_str().unwrap().into())?;
            let (_, pane) = tabs.resolve(None, Some(id))?;
            Ok(format!(
                "retitled id={id} tab={id} pane={pane} revision={}",
                tabs.revision
            ))
        }
        "term.tab.move" => {
            let id = args["id"].as_u64().unwrap();
            // Validated integers below zero clamp to the first slot; u64
            // also accepts indices above i64::MAX without narrowing them.
            let index = tabs.move_tab(id, args["index"].as_u64().unwrap_or(0))?;
            let (_, pane) = tabs.resolve(None, Some(id))?;
            Ok(format!(
                "moved id={id} index={index} tab={id} pane={pane} revision={}",
                tabs.revision
            ))
        }
        "term.tab.new" => tabs
            .open_options(
                args["cwd"].as_str().map(str::to_owned),
                args["title"].as_str().map(str::to_owned),
            )
            .map(|id| {
                let binding = tabs.binding(tabs.active_tab().active_pane);
                format!("opened id={id} {} binding={binding}", identity(&tabs))
            }),
        "term.tab.select" => {
            let id = args["id"]
                .as_u64()
                .ok_or_else(|| "internal: term verb/args desync (id)".to_string())?;
            // select wakes the event loop; refresh compares View.rendered_id
            // with active_id and uploads even without a PTY damage event.
            if tabs.select(id) {
                Ok(format!("selected id={id} {}", identity(&tabs)))
            } else {
                Err(format!("not-found: tab id={id}"))
            }
        }
        "term.tab.close" => {
            let id = args["id"]
                .as_u64()
                .ok_or_else(|| "internal: term verb/args desync (id)".to_string())?;
            let (outcome, removed) = tabs.close(id);
            let revision = tabs.revision;
            drop(tabs);
            cleanup.submit(removed.into_iter().collect());
            match outcome {
                Outcome::Unknown => Err(format!("not-found: tab id={id}")),
                Outcome::Remaining(count) => Ok(format!(
                    "closed id={id} remaining={count} revision={revision}"
                )),
                Outcome::Empty => Ok(format!("closed id={id} last revision={revision}")),
            }
        }
        "term.panes" => {
            let tab = args["tab"].as_u64();
            let (id, panes) = match tab {
                Some(id) => (id, tabs.leaves_in(id)?),
                None => (
                    if tabs.is_empty() { 0 } else { tabs.active_id() },
                    tabs.leaves(),
                ),
            };
            Ok(panes
            .iter()
            .map(|pane| {
                let g = pane.geometry;
                format!(
                    "id={} active={} cols={} rows={} child_pid={} x={} y={} w={} h={} tab={} revision={}",
                    pane.id,
                    pane.active,
                    pane.cols,
                    pane.rows,
                    pane.child_pid,
                    g.x,
                    g.y,
                    g.w,
                    g.h,
                    id,
                    tabs.revision
                )
            })
            .collect::<Vec<_>>()
            .join("\n"))
        }
        // VERIFY: term.pane.split handler — validated JSON direction, new active ID.
        "term.pane.split" => {
            let dir = parse_dir(
                args["dir"]
                    .as_str()
                    .ok_or_else(|| "internal: term verb/args desync (dir)".to_string())?,
            )?;
            tabs.split_active(dir).map(|id| {
                format!(
                    "split id={id} dir={} {}",
                    if dir == crate::panes::SplitDir::Horizontal {
                        "h"
                    } else {
                        "v"
                    },
                    identity(&tabs)
                )
            })
        }
        "term.pane.select" => {
            let id = args["id"]
                .as_u64()
                .ok_or_else(|| "internal: term verb/args desync (id)".to_string())?;
            if tabs.focus(id) {
                Ok(format!("selected id={id} {}", identity(&tabs)))
            } else {
                Err(format!("not-found: pane in active tab id={id}"))
            }
        }
        "term.pane.close" => {
            if tabs.is_empty() {
                return Err("application closing".into());
            }
            let id = tabs.active_tab().active_pane;
            let tab = tabs.active_id();
            let tab_closed = tabs.leaves().len() == 1;
            let (_, removed) = tabs.close_active();
            let count = tabs.leaves().len();
            let revision = tabs.revision;
            drop(tabs);
            cleanup.submit(removed.into_iter().collect());
            if tab_closed {
                Ok(format!(
                    "closed id={id} tab-closed tab={tab} revision={revision}"
                ))
            } else {
                Ok(format!(
                    "closed id={id} panes={count} tab={tab} revision={revision}"
                ))
            }
        }
        "term.scroll" => {
            use crate::terminal::ScrollRequest;
            let (tab, pane) = tabs.resolve(args["pane"].as_u64(), None)?;
            let selected = tabs.pane_by_id(pane).expect("resolved pane exists under set lock");
            let terminal = selected.lock().unwrap();
            let scroll = match args["to"].as_str() {
                Some("top") => ScrollRequest::Top,
                Some("bottom") => ScrollRequest::Bottom,
                _ => match args["lines"].as_i64() {
                    Some(lines) => ScrollRequest::Lines(lines),
                    None => ScrollRequest::Pages(args["page"].as_i64().unwrap()),
                },
            };
            let (display_offset, history_lines, changed) = terminal.scroll_view_state(scroll);
            drop(terminal);
            if changed {
                tabs.changed("pane.changed", tab, pane, "scrolled");
            }
            Ok(serde_json::json!({"pane": pane, "display_offset": display_offset,
                "history_lines": history_lines}).to_string())
        }
        // VERIFY: active-pane snapshot/type — selection stays under the set lock.
        "term.snapshot" | "term.type" => {
            let (tab, pane) = tabs.resolve(args["pane"].as_u64(), args["tab"].as_u64())?;
            let active = tabs
                .pane_by_id(pane)
                .expect("resolved pane exists under set lock");
            let identity = format!("tab={tab} pane={pane} revision={}", tabs.revision);
            let terminal = active.lock().unwrap();
            if verb == "term.snapshot" {
                let snapshot = terminal.capture_snapshot(
                    args["contents"].as_bool().unwrap_or(true),
                    args["scrollback_lines"].as_u64().unwrap_or(0) as usize,
                );
                drop(terminal);
                drop(tabs);
                // Onto the snapshot's own key=value header line.
                Ok(format!("{identity} {}", snapshot.render()))
            } else {
                // VERIFY: term.type extracts validated text, never the JSON envelope.
                terminal
                    .listener
                    .bus_text(
                        args["text"]
                            .as_str()
                            .ok_or_else(|| "internal: term verb/args desync (text)".to_string())?,
                    )
                    .map(|_| {
                        format!(
                            "DIAGNOSTIC synthetic keys queued; inspect input_written for actual writes {identity}"
                        )
                    })
            }
        }
        _ => Err("unknown verb; use HELP".into()),
    }
}

/// The identity a targetless verb acted on, echoed so a caller can detect
/// drift after the fact (detection, not binding): the active tab and pane
/// after the verb, and the tab-set revision. Requires a non-empty set.
fn identity(tabs: &TabSet) -> String {
    format!(
        "tab={} pane={} revision={}",
        tabs.active_id(),
        tabs.active_tab().active_pane,
        tabs.revision
    )
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
        | "term.session" | "term.props.watch" | "term.scroll" => None,
        #[cfg(test)]
        "term.test.panic" | "term.test.panic_locked" => None,
        "term.type" => Some("text"),
        "term.tab.select" | "term.tab.close" | "term.pane.select" | "term.tab.title"
        | "term.tab.move" => Some("id"),
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
    let extra: &[&str] = match verb {
        "term.snapshot" => &["pane", "tab", "contents", "scrollback_lines"],
        "term.type" => &["pane"],
        "term.scroll" => &["pane", "lines", "page", "to"],
        "term.panes" => &["tab"],
        "term.tab.new" => &["cwd", "title"],
        "term.tab.title" => &["title"],
        "term.tab.move" => &["index"],
        _ => &[],
    };
    // `request_id` rides alongside any verb's own argument: it addresses the
    // reply-replay cache in dispatch(), never the verb itself.
    if object.keys().any(|key| {
        Some(key.as_str()) != field && key != "request_id" && !extra.contains(&key.as_str())
    }) {
        return Err(format!("unexpected argument for {verb}"));
    }
    if object.contains_key("request_id") {
        args["request_id"]
            .as_str()
            .ok_or("request_id must be a string")?;
    }
    if verb == "term.scroll" {
        if ["lines", "page", "to"].iter().filter(|key| object.contains_key(**key)).count() != 1 {
            return Err("invalid-argument: exactly one of lines, page or to is required".into());
        }
        for key in ["lines", "page"] {
            if object.contains_key(key) && args[key].as_i64().is_none() {
                return Err(format!("invalid-argument: {key} must be a signed integer (i64)"));
            }
        }
        if object.contains_key("to") && !matches!(args["to"].as_str(), Some("top" | "bottom")) {
            return Err("invalid-argument: to must be top or bottom".into());
        }
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
    for key in ["pane", "tab", "scrollback_lines"] {
        if object.contains_key(key) && args[key].as_u64().is_none() {
            return Err(format!(
                "invalid-argument: {key} must be a non-negative integer (u64)"
            ));
        }
    }
    if args["scrollback_lines"].as_u64().is_some_and(|n| n > 10000) {
        return Err("invalid-argument: scrollback_lines must be 0..=10000".into());
    }
    if object.contains_key("contents") && !args["contents"].is_boolean() {
        return Err("invalid-argument: contents must be a boolean".into());
    }
    for key in ["cwd", "title"] {
        if object.contains_key(key) && !args[key].is_string() {
            return Err(format!("invalid-argument: {key} must be a string"));
        }
    }
    if let Some(cwd) = args["cwd"].as_str() {
        crate::terminal::validate_cwd(cwd)?;
    }
    if verb == "term.tab.title" && !args["title"].is_string() {
        return Err("invalid-argument: title must be a string".into());
    }
    if verb == "term.tab.move"
        && args["index"].as_u64().is_none()
        && args["index"].as_i64().is_none()
    {
        return Err("invalid-argument: index must be an integer".into());
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c6_fixture() -> Option<(Mutex<TabSet>, Cleanup, std::thread::JoinHandle<()>)> {
        if !std::path::Path::new("/opt/cosmix/bin/mix").is_file() {
            eprintln!("SKIP C6 PTY test: Mix unavailable");
            return None;
        }
        let (cleanup, worker) = Cleanup::start().unwrap();
        Some((Mutex::new(TabSet::new().unwrap()), cleanup, worker))
    }

    #[test]
    fn scroll_viewport_selectors_events_replay_and_refusals() {
        let settings = crate::config::Settings {
            config: crate::config::Config::default(),
            term: "xterm-256color",
        };
        let fixture = || Ok(crate::terminal::Terminal::from_test_vt(
            8, 3, b"old0\r\nold1\r\nlive0\r\nlive1\r\nlive2",
        ));
        let mut tabs = TabSet::with_initial(settings, None, fixture).unwrap();
        let input = tabs.active_terminal().lock().unwrap().listener.test_input_receiver();
        tabs.open_with(fixture).unwrap();
        let focus = identity(&tabs);
        let mut events = tabs.observe();
        let set = Mutex::new(tabs);
        let (cleanup, worker) = Cleanup::start().unwrap();
        let mut replies = ReplyCache::default();
        let call = |service, body: &str, replies: &mut ReplyCache| {
            super::dispatch(true, service, &set, &cleanup, replies,
                &format!("{service}.scroll"), body)
        };
        // Both service names use the same targetless implementation.
        for service in ["term", "bterm"] {
            call(service, r#"{"to":"top"}"#, &mut replies).unwrap();
            let reply = call(service, r#"{"to":"bottom"}"#, &mut replies).unwrap();
            assert_eq!(serde_json::from_str::<serde_json::Value>(&reply).unwrap(),
                serde_json::json!({"pane":2,"display_offset":0,"history_lines":2}));
        }
        assert!(events.try_recv().is_err());
        handle(&set, &cleanup, "term.props.watch", "{}").unwrap();
        let live = handle(&set, &cleanup, "term.snapshot", r#"{"pane":1}"#).unwrap();
        let body = r#"{"pane":1,"lines":1,"request_id":"scroll-once"}"#;
        let first = call("term", body, &mut replies).unwrap();
        assert_eq!(call("term", body, &mut replies).unwrap(), first);
        assert_eq!(serde_json::from_str::<serde_json::Value>(&first).unwrap(),
            serde_json::json!({"pane":1,"display_offset":1,"history_lines":2}));
        let event = events.try_recv().unwrap();
        assert_eq!((event.topic, event.tab, event.pane, event.kind), ("pane.changed", 1, 1, "scrolled"));
        assert!(events.try_recv().is_err(), "replay does not scroll or publish again");
        assert_eq!(handle(&set, &cleanup, "term.snapshot", r#"{"pane":1}"#).unwrap().split_once("--- screen ---\n").unwrap().1,
            live.split_once("--- screen ---\n").unwrap().1);
        for (body, offset) in [
            (r#"{"pane":1,"page":1}"#, 2),
            (r#"{"pane":1,"page":-1}"#, 0),
            (r#"{"pane":1,"lines":9223372036854775807}"#, 2),
            (r#"{"pane":1,"lines":9223372036854775807}"#, 2),
            (r#"{"pane":1,"lines":-9223372036854775808}"#, 0),
            (r#"{"pane":1,"page":9223372036854775807}"#, 2),
            (r#"{"pane":1,"page":-9223372036854775808}"#, 0),
            (r#"{"pane":1,"to":"top"}"#, 2),
            (r#"{"pane":1,"to":"bottom"}"#, 0),
            (r#"{"pane":1,"lines":0}"#, 0),
        ] {
            let reply = call("bterm", body, &mut replies).unwrap();
            assert_eq!(serde_json::from_str::<serde_json::Value>(&reply).unwrap()["display_offset"], offset);
        }
        assert_eq!(identity(&set.lock().unwrap()), focus);
        assert_eq!(set.lock().unwrap().active_terminal().lock().unwrap().display_offset(), 0);
        assert!(input.try_recv().is_err(), "viewport scrolling never writes PTY input");
        let mut changes = Vec::new();
        while let Ok(event) = events.try_recv() {
            changes.push(event);
        }
        assert_eq!(changes.len(), 8, "clamped and zero requests do not publish");
        assert!(changes.iter().all(|event| event.kind == "scrolled" && event.pane == 1));
        for body in ["{}", r#"{"lines":1,"page":1}"#, r#"{"lines":1,"to":"top"}"#,
            r#"{"page":1,"to":"bottom"}"#, r#"{"lines":null}"#, r#"{"lines":1.5}"#,
            r#"{"page":"1"}"#, r#"{"lines":true}"#, r#"{"to":"middle"}"#,
            r#"{"to":null}"#, r#"{"lines":9223372036854775808}"#,
            r#"{"pane":-1,"lines":1}"#, r#"{"pane":null,"lines":1}"#,
            r#"{"tab":1,"lines":1}"#, r#"{"to":"top","extra":1}"#] {
            assert!(call("term", body, &mut replies).is_err(), "accepted {body}");
        }
        assert!(call("term", r#"{"pane":999,"lines":1}"#, &mut replies).unwrap_err().starts_with("not-found:"));
        assert!(super::dispatch(false, "bterm", &set, &cleanup, &mut replies,
            "bterm.scroll", r#"{"lines":1}"#).is_err());
        assert!(call("term", r#"{"lines":2,"request_id":"scroll-once"}"#, &mut replies).is_err());
        handle(&set, &cleanup, "term.tab.close", r#"{"id":1}"#).unwrap();
        assert!(call("term", r#"{"pane":1,"lines":1}"#, &mut replies).unwrap_err().starts_with("not-found:"));
        cleanup.submit(set.lock().unwrap().shutdown());
        drop(cleanup);
        worker.join().unwrap();
    }

    #[test]
    fn selected_scrolled_pane_snapshots_stay_live_and_typing_follows_input() {
        use crate::terminal::{ScrollRequest, Terminal};
        let settings = crate::config::Settings {
            config: crate::config::Config::default(),
            term: "xterm-256color",
        };
        let fixture = || {
            Ok(Terminal::from_test_vt(
                8,
                3,
                b"old0\r\nold1\r\nlive0\r\nlive1\r\nlive2",
            ))
        };
        let mut tabs = TabSet::with_initial(settings, None, fixture).unwrap();
        let target = tabs.active_terminal();
        let input = target.lock().unwrap().listener.test_input_receiver();
        tabs.open_with(fixture).unwrap();
        let focused = tabs.active_terminal();
        let focus = identity(&tabs);
        let set = Mutex::new(tabs);
        let (cleanup, worker) = Cleanup::start().unwrap();
        for terminal in [&target, &focused] {
            terminal.lock().unwrap().scroll_view(ScrollRequest::Top);
            assert_eq!(terminal.lock().unwrap().display_offset(), 2);
        }
        for (selectors, expected) in [
            (r#""pane":1"#, "live0   \nlive1   \nlive2   \n"),
            (
                r#""tab":1,"scrollback_lines":1"#,
                "old1    \nlive0   \nlive1   \nlive2   \n",
            ),
            (
                r#""pane":1,"tab":1,"scrollback_lines":10000"#,
                "old0    \nold1    \nlive0   \nlive1   \nlive2   \n",
            ),
        ] {
            let reply =
                handle(&set, &cleanup, "term.snapshot", &format!("{{{selectors}}}")).unwrap();
            assert_eq!(reply.split_once("--- screen ---\n").unwrap().1, expected);
            assert!(reply.contains("cursor=5,2"));
            assert_eq!(target.lock().unwrap().display_offset(), 2);
            assert_eq!(identity(&set.lock().unwrap()), focus);
        }
        handle(&set, &cleanup, "term.type", r#"{"pane":1,"text":""}"#).unwrap();
        assert_eq!(target.lock().unwrap().display_offset(), 2);
        handle(&set, &cleanup, "term.type", r#"{"pane":1,"text":"x"}"#).unwrap();
        assert!(matches!(
            input.try_recv().unwrap(),
            rio_vt::event::Msg::Input(bytes) if bytes.as_ref() == b"x"
        ));
        assert_eq!(target.lock().unwrap().display_offset(), 0);
        assert_eq!(focused.lock().unwrap().display_offset(), 2);
        assert_eq!(identity(&set.lock().unwrap()), focus);
        cleanup.submit(set.lock().unwrap().shutdown());
        drop(cleanup);
        worker.join().unwrap();
    }

    #[test]
    fn c6_selectors_cross_tabs_and_type_preserves_focus() {
        let Some((set, cleanup, worker)) = c6_fixture() else {
            return;
        };
        let first = set.lock().unwrap().active_tab().active_pane;
        handle(&set, &cleanup, "term.tab.new", "").unwrap();
        let focus = identity(&set.lock().unwrap());
        let target = set.lock().unwrap().pane_by_id(first).unwrap();
        let before = target.lock().unwrap().listener.foreground_generation();
        let typed = handle(
            &set,
            &cleanup,
            "term.type",
            &format!(r#"{{"pane":{first},"text":"c6_probe"}}"#),
        )
        .unwrap();
        assert!(typed.ends_with(&format!("tab=1 pane={first} revision=2")));
        assert_eq!(identity(&set.lock().unwrap()), focus);
        assert!(target.lock().unwrap().listener.foreground_generation() > before);
        let snapshot = handle(
            &set,
            &cleanup,
            "term.snapshot",
            &format!(r#"{{"pane":{first},"contents":false}}"#),
        )
        .unwrap();
        assert!(snapshot.starts_with(&format!("tab=1 pane={first} ")));
        assert!(!snapshot.contains("--- screen ---"));
        assert!(
            handle(&set, &cleanup, "term.snapshot", r#"{"tab":1}"#)
                .unwrap()
                .contains("--- screen ---")
        );
        let panes = handle(&set, &cleanup, "term.panes", r#"{"tab":1}"#).unwrap();
        assert_eq!(panes.lines().count(), 1);
        assert!(panes.contains("tab=1"));
        assert_eq!(identity(&set.lock().unwrap()), focus);
        assert!(
            handle(
                &set,
                &cleanup,
                "term.snapshot",
                &format!(r#"{{"pane":{first},"tab":2}}"#)
            )
            .unwrap_err()
            .contains("invalid-argument")
        );
        handle(&set, &cleanup, "term.tab.close", r#"{"id":1}"#).unwrap();
        for (verb, body) in [
            ("term.snapshot", format!(r#"{{"pane":{first}}}"#)),
            ("term.type", format!(r#"{{"pane":{first},"text":""}}"#)),
            ("term.panes", r#"{"tab":1}"#.into()),
            ("term.snapshot", r#"{"tab":1}"#.into()),
        ] {
            assert!(
                handle(&set, &cleanup, verb, &body)
                    .unwrap_err()
                    .contains("not-found")
            );
        }
        cleanup.submit(set.lock().unwrap().shutdown());
        drop(cleanup);
        worker.join().unwrap();
    }

    #[test]
    fn c6_tab_new_cwd_title_and_refusal_are_atomic() {
        let Some((set, cleanup, worker)) = c6_fixture() else {
            return;
        };
        for cwd in [
            "relative",
            "/a-directory-that-does-not-exist-c6",
            "/dev/null",
        ] {
            let body = serde_json::json!({"cwd":cwd,"title":"refused"}).to_string();
            assert!(
                handle(&set, &cleanup, "term.tab.new", &body)
                    .unwrap_err()
                    .contains("invalid-argument")
            );
            assert_eq!(set.lock().unwrap().list().len(), 1);
        }
        let body = serde_json::json!({"cwd":"/", "title":"build"}).to_string();
        assert!(
            handle(&set, &cleanup, "term.tab.new", &body)
                .unwrap()
                .starts_with("opened id=2 ")
        );
        let tabs = set.lock().unwrap();
        assert_eq!(tabs.active_tab().title, "build");
        let pid = tabs.list()[1].child_pid;
        assert_eq!(
            std::fs::read_link(format!("/proc/{pid}/cwd")).unwrap(),
            std::path::Path::new("/")
        );
        drop(tabs);
        cleanup.submit(set.lock().unwrap().shutdown());
        drop(cleanup);
        worker.join().unwrap();
    }

    #[test]
    fn c6_title_move_and_replays() {
        use rio_vt::event::{EventListener, RioEvent, WindowId};
        let Some((set, cleanup, worker)) = c6_fixture() else {
            return;
        };
        let mut replies = ReplyCache::default();
        let pane = set.lock().unwrap().active_terminal();
        let listener = pane.lock().unwrap().listener.clone();
        listener.send_event(RioEvent::Title("program".into()), WindowId::from(0));
        let pin = r#"{"id":1,"title":"pinned","request_id":"pin"}"#;
        let reply = dispatch(true, &set, &cleanup, &mut replies, "term.tab.title", pin).unwrap();
        listener.send_event(RioEvent::Title("latest".into()), WindowId::from(0));
        assert!(
            handle(&set, &cleanup, "term.tabs", "")
                .unwrap()
                .contains("title=pinned")
        );
        handle(&set, &cleanup, "term.tab.title", r#"{"id":1,"title":""}"#).unwrap();
        assert_eq!(set.lock().unwrap().active_tab().title, "latest");
        assert_eq!(
            dispatch(true, &set, &cleanup, &mut replies, "term.tab.title", pin).unwrap(),
            reply
        );
        assert_eq!(
            set.lock().unwrap().active_tab().title,
            "latest",
            "replay does not repin"
        );
        let before = set.lock().unwrap().revision;
        let unsafe_title =
            serde_json::json!({"id":1,"title":format!("x\nid=7\t{}", "𝐀".repeat(100))}).to_string();
        handle(&set, &cleanup, "term.tab.title", &unsafe_title).unwrap();
        let listing = handle(&set, &cleanup, "term.tabs", "").unwrap();
        assert_eq!(listing.lines().count(), 1);
        assert!(listing.contains("title=xid=7"));
        assert!(set.lock().unwrap().active_tab().title.len() <= 256);
        assert_eq!(set.lock().unwrap().revision, before);
        assert!(
            handle(
                &set,
                &cleanup,
                "term.tab.title",
                r#"{"id":999,"title":"x"}"#
            )
            .unwrap_err()
            .contains("not-found")
        );
        handle(&set, &cleanup, "term.tab.new", "").unwrap();
        let moved = r#"{"id":1,"index":18446744073709551615,"request_id":"move"}"#;
        let reply = dispatch(true, &set, &cleanup, &mut replies, "term.tab.move", moved).unwrap();
        assert!(reply.contains("index=1"));
        assert_eq!(
            set.lock()
                .unwrap()
                .list()
                .iter()
                .map(|t| t.id)
                .collect::<Vec<_>>(),
            [2, 1]
        );
        assert_eq!(set.lock().unwrap().active_id(), 2);
        handle(&set, &cleanup, "term.tab.move", r#"{"id":1,"index":-1}"#).unwrap();
        assert_eq!(
            dispatch(true, &set, &cleanup, &mut replies, "term.tab.move", moved).unwrap(),
            reply
        );
        assert_eq!(set.lock().unwrap().list()[0].id, 1);
        assert!(
            handle(&set, &cleanup, "term.tab.move", r#"{"id":999,"index":0}"#)
                .unwrap_err()
                .contains("not-found")
        );
        cleanup.submit(set.lock().unwrap().shutdown());
        drop(cleanup);
        worker.join().unwrap();
    }

    #[test]
    fn c6_watch_events_cover_local_mutations_and_noops() {
        let Some((set, cleanup, worker)) = c6_fixture() else {
            return;
        };
        let mut events = set.lock().unwrap().observe();
        let reply = super::handle("bterm", &set, &cleanup, "term.props.watch", "{}").unwrap();
        let reply: serde_json::Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(
            reply["topics"],
            serde_json::json!([
                "bterm.tabs.changed",
                "bterm.pane.changed",
                "bterm.title.changed"
            ])
        );
        assert!(handle(&set, &cleanup, "term.props.watch", r#"{"caller":"spoof"}"#).is_err());
        assert!(events.try_recv().is_err());
        let mut tabs = set.lock().unwrap();
        let pane = tabs.split_active(crate::panes::SplitDir::Vertical).unwrap();
        tabs.focus(1);
        tabs.resized(1, 100, 30);
        tabs.set_title(1, "label".into()).unwrap();
        let other = tabs.open().unwrap();
        tabs.cycle(true);
        tabs.cycle(false);
        tabs.move_tab(1, 1).unwrap();
        tabs.select(1);
        tabs.focus(pane);
        cleanup.submit(tabs.close_active().1.into_iter().collect());
        cleanup.submit(tabs.close(other).1.into_iter().collect());
        let mut got = Vec::new();
        while let Ok(event) = events.try_recv() {
            got.push(event);
        }
        for (index, event) in got.iter().enumerate() {
            if event.topic == "tabs.changed" && event.kind == "selected" {
                let pane_event = &got[index + 1];
                assert_eq!(
                    (pane_event.topic, pane_event.kind),
                    ("pane.changed", "selected")
                );
                assert_eq!((pane_event.tab, pane_event.pane), (event.tab, event.pane));
            }
        }
        for (topic, kind) in [
            ("tabs.changed", "added"),
            ("tabs.changed", "removed"),
            ("tabs.changed", "moved"),
            ("tabs.changed", "selected"),
            ("tabs.changed", "retitled"),
            ("title.changed", "retitled"),
            ("pane.changed", "added"),
            ("pane.changed", "removed"),
            ("pane.changed", "selected"),
            ("pane.changed", "resized"),
        ] {
            assert!(
                got.iter().any(|e| e.topic == topic && e.kind == kind),
                "{topic}/{kind}: {got:?}"
            );
        }
        assert!(
            got.windows(2)
                .all(|pair| pair[0].revision < pair[1].revision)
        );
        let body = got[0].body();
        assert_eq!(body.as_object().unwrap().len(), 4);
        assert_eq!(body["tab"], 1);
        assert_eq!(body["pane"], pane);
        tabs.select(1);
        tabs.focus(1);
        tabs.resized(1, 100, 30);
        tabs.move_tab(1, 0).unwrap();
        tabs.set_title(1, "label".into()).unwrap();
        assert!(events.try_recv().is_err(), "no-op mutations stay quiet");
        cleanup.submit(tabs.shutdown());
        drop(tabs);
        drop(cleanup);
        worker.join().unwrap();
    }

    #[test]
    fn c6_optional_argument_validation() {
        for (verb, body) in [
            ("term.snapshot", r#"{"pane":null}"#),
            ("term.snapshot", r#"{"tab":-1}"#),
            ("term.snapshot", r#"{"contents":1}"#),
            ("term.snapshot", r#"{"scrollback_lines":10001}"#),
            ("term.snapshot", r#"{"scrollback_lines":-1}"#),
            ("term.snapshot", r#"{"scrollback_lines":1.5}"#),
            ("term.type", r#"{"pane":"1","text":""}"#),
            ("term.panes", r#"{"tab":null}"#),
            ("term.tab.new", r#"{"title":null}"#),
            ("term.tab.new", r#"{"cwd":false}"#),
            ("term.tab.title", r#"{"id":1}"#),
            ("term.tab.move", r#"{"id":1,"index":1.5}"#),
            ("term.tab.move", r#"{"id":1}"#),
        ] {
            assert!(parse_args(verb, body).is_err(), "{verb} {body}");
        }
        for n in [0, 10000] {
            assert!(parse_args("term.snapshot", &format!(r#"{{"scrollback_lines":{n}}}"#)).is_ok());
        }
    }

    /// The existing suite predates the service-name parameter and exercises
    /// the canonical `term` frontend, so it reads unchanged through this
    /// shim. `bterm`'s own routing is covered by the namespace tests below,
    /// which call the real `super::dispatch` with a service name.
    fn dispatch(
        open: bool,
        set: &Mutex<TabSet>,
        cleanup: &Cleanup,
        replies: &mut ReplyCache,
        verb: &str,
        body: &str,
    ) -> Result<String, String> {
        super::dispatch(open, CANONICAL, set, cleanup, replies, verb, body)
    }

    fn diagnostic(verb: &str) -> Result<String, String> {
        super::diagnostic(CANONICAL, verb)
    }

    fn handle(
        set: &Mutex<TabSet>,
        cleanup: &Cleanup,
        verb: &str,
        body: &str,
    ) -> Result<String, String> {
        super::handle(CANONICAL, set, cleanup, verb, body)
    }
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

    struct Quiet(tokio::sync::mpsc::Receiver<BoundedIncomingEvent>);
    impl Incoming for Quiet {
        fn next(&mut self) -> impl std::future::Future<Output = Option<BoundedIncomingEvent>> {
            self.0.recv()
        }
    }
    struct Mute;
    impl Peer for Mute {
        fn changed(
            &self,
            _: &str,
            _: Change,
        ) -> impl std::future::Future<Output = ()> + Send + 'static {
            std::future::ready(())
        }
        async fn reply(&self, _: &IncomingCommand, _: u8, _: &str) {}
        fn completed(
            &self,
            _: CompletionNote,
        ) -> impl std::future::Future<Output = ()> + Send + 'static {
            std::future::ready(())
        }
    }

    /// Event-driven law: an idle terminal's Bus loop must not wake. Every
    /// input stays open and silent for a window that the old 100 ms re-poll
    /// would have ticked through several times; the only turn allowed is the
    /// one the last-tab close causes, and that close must end the loop.
    #[test]
    fn idle_bus_loop_takes_no_turns_until_the_last_tab_closes() {
        if !std::path::Path::new("/opt/cosmix/bin/mix").is_file() {
            eprintln!("SKIP idle Bus loop test: Mix unavailable");
            return;
        }
        let set = Arc::new(Mutex::new(TabSet::new().unwrap()));
        let (cleanup, worker) = Cleanup::start().unwrap();
        // Both senders stay alive, so neither branch retires with a `None`.
        let (_notes, mut notify_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_commands, commands) = tokio::sync::mpsc::channel(1);
        let (done_tx, done) = std::sync::mpsc::channel();
        let serving = {
            let (set, cleanup) = (set.clone(), cleanup.clone());
            std::thread::spawn(move || {
                let turns = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        let mut tasks = tokio::task::JoinSet::new();
                        let mut incoming = Quiet(commands);
                        serve(CANONICAL, &set, &cleanup, &mut notify_rx, &mut tasks, &mut incoming, &Mute).await
                    });
                done_tx.send(turns).unwrap();
            })
        };
        // Quiet window: 3.5 ticks of the retired re-poll.
        assert!(
            done.recv_timeout(Duration::from_millis(350)).is_err(),
            "loop ended while the terminal still had a tab"
        );
        cleanup.submit(set.lock().unwrap().shutdown());
        let turns = done
            .recv_timeout(Duration::from_secs(2))
            .expect("closing the last tab must wake and end the loop");
        assert_eq!(turns, 1, "an idle loop woke without an event");
        serving.join().unwrap();
        drop(cleanup);
        worker.join().unwrap();
    }

    /// A set already empty when the loop starts is never waited on.
    #[test]
    fn bus_loop_on_an_empty_set_returns_without_a_turn() {
        if !std::path::Path::new("/opt/cosmix/bin/mix").is_file() {
            eprintln!("SKIP empty Bus loop test: Mix unavailable");
            return;
        }
        let set = Mutex::new(TabSet::new().unwrap());
        let (cleanup, worker) = Cleanup::start().unwrap();
        cleanup.submit(set.lock().unwrap().shutdown());
        let (_notes, mut notify_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_commands, commands) = tokio::sync::mpsc::channel(1);
        let turns = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut tasks = tokio::task::JoinSet::new();
                serve(CANONICAL, &set, &cleanup, &mut notify_rx, &mut tasks, &mut Quiet(commands), &Mute).await
            });
        assert_eq!(turns, 0);
        drop(cleanup);
        worker.join().unwrap();
    }

    /// Exact replies for the tab verbs' identity echo, the no-bump rule for
    /// select, both tab.close forms, and the closing latch after the last tab.
    #[test]
    fn tab_verbs_echo_exact_identity_and_the_last_close_latches() {
        if !std::path::Path::new("/opt/cosmix/bin/mix").is_file() {
            eprintln!("SKIP tab identity test: Mix unavailable");
            return;
        }
        let set = Mutex::new(TabSet::new().unwrap());
        let (cleanup, worker) = Cleanup::start().unwrap();
        let revision = || set.lock().unwrap().revision;
        // Every structural verb must bump the revision by exactly one: the
        // expected values come from `before`, not a read after the call, so a
        // verb that stopped bumping fails here.
        let before = revision();
        assert_eq!(
            handle(&set, &cleanup, "term.tab.new", "").unwrap(),
            format!("opened id=2 tab=2 pane=2 revision={} binding=unavailable", before + 1)
        );
        assert_eq!(revision(), before + 1);
        let tabs = handle(&set, &cleanup, "term.tabs", "").unwrap();
        let lines: Vec<Vec<(&str, &str)>> = tabs
            .lines()
            .map(|line| {
                line.split(' ')
                    .map(|pair| pair.split_once('=').expect("key=value token"))
                    .collect()
            })
            .collect();
        assert_eq!(lines.len(), 2);
        for (line, (id, active)) in lines.iter().zip([("1", "false"), ("2", "true")]) {
            let keys: Vec<_> = line.iter().map(|(key, _)| *key).collect();
            assert_eq!(
                keys,
                ["id", "active", "title", "cols", "rows", "child_pid", "revision"],
                "{tabs}"
            );
            assert_eq!(line[0], ("id", id));
            assert_eq!(line[1], ("active", active));
            assert_eq!(line[2], ("title", "mix"));
            assert_eq!(line[6], ("revision", (before + 1).to_string().as_str()));
        }
        // Selecting does not bump the revision: drift from a select shows in
        // tab=/pane=, never in revision=.
        let before = revision();
        assert_eq!(
            handle(&set, &cleanup, "term.tab.select", r#"{"id":1}"#).unwrap(),
            format!("selected id=1 tab=1 pane=1 revision={before}")
        );
        assert_eq!(revision(), before);
        assert_eq!(
            handle(&set, &cleanup, "term.tab.close", r#"{"id":2}"#).unwrap(),
            format!("closed id=2 remaining=1 revision={}", before + 1)
        );
        assert_eq!(revision(), before + 1);
        let before = revision();
        assert_eq!(
            handle(&set, &cleanup, "term.tab.close", r#"{"id":1}"#).unwrap(),
            format!("closed id=1 last revision={}", before + 1)
        );
        assert_eq!(revision(), before + 1);
        // The last close latches: no verb can reopen a closing terminal.
        assert_eq!(
            handle(&set, &cleanup, "term.tab.new", "").unwrap_err(),
            "application closing"
        );
        assert!(set.lock().unwrap().is_empty());
        drop(cleanup);
        worker.join().unwrap();
    }

    /// The property serve() relies on to close the check-then-wait window:
    /// a last-tab close with nobody waiting leaves a permit, so a wait that
    /// starts afterwards completes at once instead of sleeping forever.
    #[test]
    fn a_close_before_the_wait_is_not_lost() {
        if !std::path::Path::new("/opt/cosmix/bin/mix").is_file() {
            eprintln!("SKIP emptied permit test: Mix unavailable");
            return;
        }
        let set = Mutex::new(TabSet::new().unwrap());
        let (cleanup, worker) = Cleanup::start().unwrap();
        let emptied = set.lock().unwrap().emptied();
        cleanup.submit(set.lock().unwrap().shutdown());
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                tokio::time::timeout(Duration::from_millis(100), emptied.notified())
                    .await
                    .expect("the close's permit was lost");
            });
        drop(cleanup);
        worker.join().unwrap();
    }

    struct Recorder(std::sync::mpsc::Sender<(String, u8, String)>);
    impl Peer for Recorder {
        fn changed(
            &self,
            service: &str,
            change: Change,
        ) -> impl std::future::Future<Output = ()> + Send + 'static {
            let sender = self.0.clone();
            let topic = format!("{service}.{}", change.topic);
            async move {
                let _ = sender.send((topic, 0, change.body().to_string()));
            }
        }
        async fn reply(&self, command: &IncomingCommand, rc: u8, body: &str) {
            let _ = self.0.send((command.command.clone(), rc, body.into()));
        }
        fn completed(
            &self,
            _: CompletionNote,
        ) -> impl std::future::Future<Output = ()> + Send + 'static {
            std::future::ready(())
        }
    }

    fn command(verb: &str, body: &str) -> BoundedIncomingEvent {
        BoundedIncomingEvent::Command(IncomingCommand {
            from: "test".into(),
            command: verb.into(),
            id: None,
            args: serde_json::Value::Null,
            body: body.into(),
            headers: Default::default(),
        })
    }

    /// Command feed, recorded (verb, rc, body) replies, and the serve thread.
    type Serving = (
        tokio::sync::mpsc::Sender<BoundedIncomingEvent>,
        std::sync::mpsc::Receiver<(String, u8, String)>,
        std::thread::JoinHandle<()>,
    );

    /// Run serve() on its own thread with a recording peer; the caller feeds
    /// commands and reads replies.
    fn serving(set: &Arc<Mutex<TabSet>>, cleanup: &Cleanup) -> Serving {
        let (commands_tx, commands) = tokio::sync::mpsc::channel(4);
        let (replies_tx, replies) = std::sync::mpsc::channel();
        let (set, cleanup) = (set.clone(), cleanup.clone());
        let thread = std::thread::spawn(move || {
            let (_notes, mut notify_rx) = tokio::sync::mpsc::unbounded_channel();
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let mut tasks = tokio::task::JoinSet::new();
                    let mut incoming = Quiet(commands);
                    serve(
                        CANONICAL,
                        &set,
                        &cleanup,
                        &mut notify_rx,
                        &mut tasks,
                        &mut incoming,
                        &Recorder(replies_tx),
                    )
                    .await;
                });
        });
        (commands_tx, replies, thread)
    }

    #[test]
    fn c6_serve_publishes_local_changes_and_drains_last_close() {
        let Some((set, cleanup, worker)) = c6_fixture() else {
            return;
        };
        let set = Arc::new(set);
        let (commands, replies, thread) = serving(&set, &cleanup);
        // Real shell startup may emit OSC titles. Ignore those independent
        // events while requiring each requested event within one deadline.
        let receive = |expected: &str| {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            loop {
                let (verb, rc, body) = replies
                    .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                    .unwrap();
                if verb == expected {
                    break (verb, rc, body);
                }
                assert!(matches!(
                    verb.as_str(),
                    "term.tabs.changed" | "term.title.changed"
                ));
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&body).unwrap()["kind"],
                    "retitled"
                );
            }
        };
        commands
            .blocking_send(command("term.props.watch", ""))
            .unwrap();
        let (verb, rc, _) = receive("term.props.watch");
        assert_eq!((verb.as_str(), rc), ("term.props.watch", 0));
        set.lock().unwrap().resized(1, 100, 30);
        let (topic, rc, body) = receive("term.pane.changed");
        assert_eq!((topic.as_str(), rc), ("term.pane.changed", 0));
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["kind"], "resized");
        commands
            .blocking_send(command("term.tab.close", r#"{"id":1}"#))
            .unwrap();
        let (verb, rc, _) = receive("term.tab.close");
        assert_eq!((verb.as_str(), rc), ("term.tab.close", 0));
        for expected in ["term.pane.changed", "term.tabs.changed"] {
            let (topic, _, body) = receive(expected);
            assert_eq!(topic, expected);
            let body: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(body["kind"], "removed");
        }
        thread.join().unwrap();
        drop(cleanup);
        worker.join().unwrap();
    }

    /// Through the real serve() path: a verb that panics outside the set
    /// lock answers rc 10 with the fixed message, and the NEXT command is
    /// still served. Removing the boundary kills the Bus thread and the
    /// second reply never comes.
    #[test]
    fn a_panicking_verb_is_answered_and_the_lane_keeps_serving() {
        if !std::path::Path::new("/opt/cosmix/bin/mix").is_file() {
            eprintln!("SKIP panic continuation test: Mix unavailable");
            return;
        }
        let set = Arc::new(Mutex::new(TabSet::new().unwrap()));
        let (cleanup, worker) = Cleanup::start().unwrap();
        let (commands, replies, serving) = serving(&set, &cleanup);
        commands.blocking_send(command("term.test.panic", "")).unwrap();
        commands.blocking_send(command("term.tabs", "")).unwrap();
        let wait = Duration::from_secs(5);
        assert_eq!(
            replies.recv_timeout(wait).unwrap(),
            ("term.test.panic".into(), 10, HANDLER_PANICKED.into())
        );
        let (verb, rc, body) = replies.recv_timeout(wait).expect("lane stopped serving after a panic");
        assert_eq!((verb.as_str(), rc), ("term.tabs", 0));
        assert!(body.starts_with("id=1 "), "{body}");
        assert!(!set.is_poisoned());
        cleanup.submit(set.lock().unwrap().shutdown());
        serving.join().unwrap();
        drop(cleanup);
        worker.join().unwrap();
    }

    const ABORT_CHILD: &str = "COSMIX_TERM_TEST_ABORT_CHILD";

    /// A verb that panics while holding the tab-set lock must take the whole
    /// process down by SIGABRT, not carry on over torn state. Runs itself in
    /// a child process (the abort would otherwise kill the test binary).
    #[test]
    fn a_verb_that_poisons_the_tab_set_aborts_the_process() {
        if !std::path::Path::new("/opt/cosmix/bin/mix").is_file() {
            eprintln!("SKIP poison abort test: Mix unavailable");
            return;
        }
        if std::env::var_os(ABORT_CHILD).is_some() {
            // The abort is the expected outcome here; never leave a core
            // file in the caller's working directory for it.
            let none = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
            // SAFETY: setrlimit reads one live rlimit.
            unsafe { libc::setrlimit(libc::RLIMIT_CORE, &none) };
            let set = Arc::new(Mutex::new(TabSet::new().unwrap()));
            let (cleanup, _worker) = Cleanup::start().unwrap();
            let (commands, replies, _serving) = serving(&set, &cleanup);
            commands.blocking_send(command("term.test.panic_locked", "")).unwrap();
            let reply = replies.recv_timeout(Duration::from_secs(10));
            eprintln!("CHILD SURVIVED the poisoning verb; reply: {reply:?}");
            std::process::exit(3);
        }
        use std::os::unix::process::ExitStatusExt;
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "bus::tests::a_verb_that_poisons_the_tab_set_aborts_the_process",
                "--nocapture",
            ])
            .env(ABORT_CHILD, "1")
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.signal(), Some(libc::SIGABRT), "{:?}\n{stderr}", output.status);
        assert!(
            stderr.contains("\"term.test.panic_locked\" panicked while holding the tab set; aborting"),
            "{stderr}"
        );
    }

    /// A handler panic outside the shared lock is an ordinary error reply and
    /// the lock stays usable; one that unwinds through the lock is Torn.
    #[test]
    fn panic_boundary_answers_clean_panics_and_flags_torn_state() {
        let set = Mutex::new(0u32);
        assert_eq!(guard(&set, || Ok("fine".into())), Ok(Ok("fine".into())));
        assert_eq!(guard(&set, || Err("refused".into())), Ok(Err("refused".into())));
        assert_eq!(
            guard(&set, || panic!("outside the lock")),
            Ok(Err(HANDLER_PANICKED.into()))
        );
        assert!(!set.is_poisoned());
        *set.lock().unwrap() += 1;
        assert_eq!(
            guard(&set, || {
                let _held = set.lock().unwrap();
                panic!("under the lock")
            }),
            Err(Torn)
        );
        assert!(set.is_poisoned());
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
        let revision = || set.lock().unwrap().revision;
        // Every reply echoes the identity it acted on (tab, pane, revision).
        assert_eq!(
            handle(&set, &cleanup, "term.pane.split", r#"{"dir":"v"}"#).unwrap(),
            format!("split id=2 dir=v tab=1 pane=2 revision={}", revision())
        );
        let panes = handle(&set, &cleanup, "term.panes", "").unwrap();
        assert_eq!(panes.lines().count(), 2);
        for line in panes.lines() {
            assert!(
                line.ends_with(&format!(" tab=1 revision={}", revision())),
                "{line}"
            );
        }
        assert!(
            handle(&set, &cleanup, "term.snapshot", "")
                .unwrap()
                .starts_with(&format!("tab=1 pane=2 revision={} cols=", revision()))
        );
        assert!(
            handle(&set, &cleanup, "term.type", r#"{"text":""}"#)
                .unwrap()
                .ends_with(&format!(" tab=1 pane=2 revision={}", revision()))
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
        let closed = handle(&set, &cleanup, "term.pane.close", "").unwrap();
        assert_eq!(
            closed,
            format!("closed id=2 panes=1 tab=1 revision={}", revision())
        );
        assert_eq!(
            handle(
                &set,
                &cleanup,
                "term.pane.select",
                &format!(r#"{{"id":{original}}}"#)
            )
            .unwrap(),
            format!("selected id={original} tab=1 pane={original} revision={}", revision())
        );
        let closed = handle(&set, &cleanup, "term.pane.close", "").unwrap();
        assert_eq!(
            closed,
            format!("closed id={original} tab-closed tab=1 revision={}", revision())
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
        assert!(
            dispatch(false, &set, &cleanup, &mut replies, "HELP", "")
                .unwrap()
                .contains("posture=strict")
        );
        // Open posture: the full verb set answers on the global name, and
        // term.session names the posture instead of leaving it to inference.
        assert!(dispatch(true, &set, &cleanup, &mut replies, "term.tabs", "").is_ok());
        let session: serde_json::Value = serde_json::from_str(
            &dispatch(true, &set, &cleanup, &mut replies, "term.session", "").unwrap(),
        )
        .unwrap();
        assert_eq!(session["posture"], "mesh-open");
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
        // tab.new says whether the new pane got a native binding; this set
        // was built without a native session, so it cannot have one.
        let opened = dispatch(true, &set, &cleanup, &mut replies, "term.tab.new", "").unwrap();
        assert!(opened.ends_with(" binding=unavailable"), "{opened}");
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

    /// D1: the wire namespace follows the frontend name, the handlers do not.
    /// `canonical_verb` answers Some(canonical) for a verb this frontend
    /// serves, and None for a namespace it must not answer.
    fn routed(service: &str, verb: &str) -> Option<String> {
        canonical_verb(service, verb).map(|v| v.into_owned())
    }

    #[test]
    fn a_service_prefix_is_rewritten_to_the_canonical_one() {
        assert_eq!(routed("bterm", "bterm.tab.new").as_deref(), Some("term.tab.new"));
        assert_eq!(routed("bterm", "bterm.pane.split").as_deref(), Some("term.pane.split"));
        // Prefixless verbs pass through, whichever name we serve under.
        for service in ["term", "bterm"] {
            assert_eq!(routed(service, "INFO").as_deref(), Some("INFO"));
            assert_eq!(routed(service, "HELP").as_deref(), Some("HELP"));
        }
        // Serving as `term` is the identity, so the iced frontend pays
        // nothing for bterm existing.
        assert_eq!(routed("term", "term.tabs").as_deref(), Some("term.tabs"));
    }

    /// A caller talking to bterm must not reach a handler by sending the
    /// OTHER frontend's namespace: `term.tab.new` at bterm is an unknown
    /// verb, not a hidden alias. Both names routing to the same instance is
    /// exactly the collision D1 exists to prevent.
    #[test]
    fn the_other_frontends_namespace_is_not_an_alias() {
        assert_eq!(routed("bterm", "term.tab.new"), None);
        // …and that is rejected at the argument boundary, because a verb only
        // reaches a handler after parse_args accepts it. The guard here is
        // that nothing REWRITES it into the served namespace.
        assert_eq!(routed("bterm", "termite.tab.new"), None);
        assert_eq!(routed("bterm", "bterm").as_deref(), Some("bterm"));
    }

    /// The whole D1 point, end to end through the real dispatch: a bterm
    /// frontend answers `bterm.*` and does NOT answer `term.*`. If it
    /// answered both, the two frontends would still collide on every verb a
    /// mesh caller sends to the name `term`, which is exactly what the rename
    /// exists to prevent — and the A/B in T5 would be measuring one terminal
    /// wearing two hats.
    #[test]
    fn bterm_serves_its_own_namespace_and_refuses_terms() {
        if !std::path::Path::new("/opt/cosmix/bin/mix").is_file() {
            eprintln!("SKIP bterm namespace test: Mix unavailable");
            return;
        }
        let set = Mutex::new(TabSet::new().unwrap());
        let (cleanup, worker) = Cleanup::start().unwrap();
        let mut replies = ReplyCache::default();
        let mut call = |service: &str, verb: &str| {
            super::dispatch(true, service, &set, &cleanup, &mut replies, verb, "")
        };
        assert!(call("bterm", "bterm.tabs").is_ok());
        assert_eq!(
            call("bterm", "term.tabs").unwrap_err(),
            "unknown verb; use HELP",
            "bterm must not answer the iced frontend's namespace"
        );
        // …and the mirror image, so the guard is not one-sided: the iced
        // frontend answers `term.*` and not `bterm.*`.
        assert!(call("term", "term.tabs").is_ok());
        assert_eq!(call("term", "bterm.tabs").unwrap_err(), "unknown verb; use HELP");
        // HELP is prefixless, so it answers under either name — and names
        // the verbs that name's callers can actually send.
        assert!(call("bterm", "HELP").unwrap().contains("bterm.tab.new"));
        assert!(call("term", "HELP").unwrap().contains("term.tab.new"));
        drop(cleanup);
        let _ = worker.join();
    }

    /// HELP advertises verb names a caller can actually send. The loop is the
    /// point: it fails if a future HELP edit spells a verb in a way the
    /// rewrite misses, rather than shipping an unroutable name in the docs
    /// every agent reads first.
    #[test]
    fn help_renames_every_verb() {
        let rendered = help("bterm");
        for (at, _) in rendered.match_indices("term.") {
            assert!(
                at > 0 && rendered.as_bytes()[at - 1] == b'b',
                "HELP still advertises a bare `term.` verb at byte {at}: {:?}",
                &rendered[at.saturating_sub(40)..(at + 20).min(rendered.len())]
            );
        }
        assert!(rendered.contains("bterm.tab.new"));
        assert!(rendered.starts_with("bterm: "));
        // Serving as `term` renders the canonical text unchanged.
        assert_eq!(help(CANONICAL), HELP);
        for service in ["term", "bterm"] {
            let rendered = help(service);
            for suffix in [
                "snapshot",
                "scroll",
                "type",
                "panes",
                "tab.new",
                "tab.title",
                "tab.move",
                "props.watch",
            ] {
                let name = format!("{service}.{suffix}");
                assert!(rendered.contains(&name), "HELP missing {name}");
                assert_eq!(routed(service, &name), Some(format!("term.{suffix}")));
            }
            for arg in [
                "pane?",
                "lines?",
                "page?",
                "to?",
                "tab?",
                "contents?",
                "scrollback_lines?",
                "cwd?",
                "title?",
                "index:",
            ] {
                assert!(rendered.contains(arg), "HELP missing {arg}");
            }
        }
    }
}
