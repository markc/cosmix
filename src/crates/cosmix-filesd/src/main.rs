//! `cosmix-filesd` — the files-as-truth markdown corpus daemon.
//!
//! S4 makes it an Bus citizen. A single **index-writer thread** owns the `Store`
//! (a rusqlite `Connection` is `Send` but `!Sync`, so it must have exactly one
//! owner); the `notify` watcher, the periodic reconcile backstop, and the Bus
//! command loop are all producers that only ever SEND `WriterCmd`s to it — giving
//! serial-by-construction index writes. Verbs: `filesd.{list,read,save,move,delete,
//! search,changes}` + read-only `filesd.props.{get,list,describe}`.
//!
//! `filesd.save` is the sole id-minting write path (`ensure_id` → atomic write →
//! re-ingest → upsert). Self-write suppression is structural: a save upserts the
//! index, and the watcher's debounced `Reconcile` is queued behind it in the one
//! channel, so by the time it runs the on-disk hash already equals the index hash
//! and the reconcile is a no-op. modseq + ids cross Bus as JSON strings (f64).

mod config;
mod delegation;
mod props;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use cosmix_client::{IncomingCommand, NodedClient};
use cosmix_files::reconcile::{ReconcileReport, reconcile};
use cosmix_files::store::Store;
use cosmix_files::{atomic, frontmatter, ingest};
use notify::Watcher;
use serde_json::json;
use tokio::sync::{mpsc, oneshot};

/// Debounce window after a filesystem event before reconciling.
const DEBOUNCE: Duration = Duration::from_millis(250);

/// Cap on the body returned by `filesd.read` (avoid slurping a huge synced file
/// into memory + the Bus reply). Larger bodies report `body_truncated: true`.
const MAX_READ_BODY: u64 = 4 * 1024 * 1024;

/// End-to-end admission bounds. Writer commands include full Bus save bodies;
/// push jobs include full file bodies and may remain owned during a long retry.
const WRITER_QUEUE_CAPACITY: usize = 64;
const PUSH_QUEUE_CAPACITY: usize = 8;

#[derive(Parser)]
#[command(
    name = "cosmix-filesd",
    version,
    about = "Files-as-truth markdown corpus daemon"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Walk the corpus once, update the index, print a report, and exit.
    Reconcile {
        /// Config file (flat key: value); CLI flags below override it.
        #[arg(long, short = 'c')]
        config: Option<PathBuf>,
        #[arg(long)]
        root: Option<PathBuf>,
        #[arg(long)]
        corpus: Option<String>,
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Run the daemon: watch + periodic reconcile + serve filesd.* over Bus.
    Serve {
        /// Config file (flat key: value); CLI flags below override it. The systemd
        /// template runs `serve -c %E/cosmix/filesd/<corpus>.conf.mix`.
        #[arg(long, short = 'c')]
        config: Option<PathBuf>,
        #[arg(long)]
        root: Option<PathBuf>,
        #[arg(long)]
        corpus: Option<String>,
        #[arg(long)]
        db: Option<PathBuf>,
        /// Periodic reconcile interval, seconds (the correctness backstop).
        #[arg(long)]
        interval_secs: Option<u64>,
        /// Bus service name to register as (default `filesd-<corpus_id>`).
        #[arg(long)]
        bus_service: Option<String>,
    },
}

fn load_config_file(
    path: Option<&Path>,
) -> anyhow::Result<Option<std::collections::HashMap<String, String>>> {
    match path {
        Some(p) => Ok(Some(config::parse(&std::fs::read_to_string(p)?))),
        None => Ok(None),
    }
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().cmd {
        Cmd::Reconcile {
            config,
            root,
            corpus,
            db,
        } => {
            let file = load_config_file(config.as_deref())?;
            let cfg =
                config::resolve(file, root, corpus, db, None, None).map_err(anyhow::Error::msg)?;
            let mut store = open_store(&cfg.db, &cfg.corpus_id)?;
            print_report(&reconcile(&cfg.root, &mut store, now_ms())?);
            Ok(())
        }
        Cmd::Serve {
            config,
            root,
            corpus,
            db,
            interval_secs,
            bus_service,
        } => {
            // Read the raw text once: fs-mode parses repeated `place:` lines from it
            // (the flat HashMap would collide on the duplicate key), corpus mode uses
            // the parsed map.
            let text = match config.as_deref() {
                Some(p) => Some(std::fs::read_to_string(p)?),
                None => None,
            };
            let file = text.as_deref().map(config::parse);
            let is_fs = file
                .as_ref()
                .map(|f| config::mode_of(f) == "fs")
                .unwrap_or(false);
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            if is_fs {
                let text = text.expect("fs-mode implies a config file was read");
                let cfg = config::resolve_fs(&text, bus_service).map_err(anyhow::Error::msg)?;
                rt.block_on(serve_fs(cfg))
            } else {
                let cfg = config::resolve(file, root, corpus, db, interval_secs, bus_service)
                    .map_err(anyhow::Error::msg)?;
                rt.block_on(serve(cfg))
            }
        }
    }
}

fn open_store(db: &Path, corpus: &str) -> anyhow::Result<Store> {
    if let Some(parent) = db.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    Ok(Store::open(db, corpus)?)
}

async fn serve(cfg: config::CorpusConfig) -> anyhow::Result<()> {
    let config::CorpusConfig {
        root,
        corpus_id: corpus,
        db,
        interval_secs,
        bus_service,
        delegated_peers,
    } = cfg;
    let store = open_store(&db, &corpus)?;
    let (tx, rx) = mpsc::channel::<WriterCmd>(WRITER_QUEUE_CAPACITY);
    // Full-content push ownership is bounded here. On saturation the
    // Store-owning writer retains only the durable retry key and stays live;
    // the next reconcile rebuilds the body from disk.
    let (push_tx, push_rx) = mpsc::channel::<PushJob>(PUSH_QUEUE_CAPACITY);
    // Failed work is represented by keys only; the next reconcile rebuilds
    // each body from the durable corpus state. This cannot retain file bodies.
    let failed_pushes = Arc::new(StdMutex::new(BTreeSet::new()));
    // The Bus loop publishes the current broker client here so the pusher can use it
    // for outbound calls without owning the (per-reconnect) connection.
    let (client_tx, client_rx) = tokio::sync::watch::channel::<Option<Arc<NodedClient>>>(None);
    // Broker connectivity, read by the writer to gate the push cursor: while
    // disconnected the cursor must NOT advance, so changes (incl. purges) replay on
    // reconnect instead of being dropped.
    let (connected_tx, connected_rx) = tokio::sync::watch::channel(false);

    // The ONE owner of the Store: a dedicated OS thread (sqlite + reconcile are
    // blocking; keep them off the async executor). It cold-starts a reconcile, then
    // serves WriterCmds serially, emitting indexd push jobs as a side effect.
    let writer = Writer {
        root: root.clone(),
        corpus_id: corpus.clone(),
        started_at: cosmix_buildinfo::now_rfc3339(),
        started: Instant::now(),
        store,
        push_tx,
        failed_pushes: failed_pushes.clone(),
        last_pushed: 0,
        connected: connected_rx,
        delegated_peers,
    };
    std::thread::spawn(move || writer.run(rx));

    tokio::spawn(run_pusher(
        push_rx,
        client_rx,
        corpus.clone(),
        failed_pushes,
    )); // → indexd.index_file
    spawn_watcher(root.clone(), tx.clone())?; // notify (filtered, debounced) → Reconcile
    spawn_ticker(interval_secs, tx.clone()); // periodic backstop → Reconcile

    eprintln!(
        "cosmix-filesd: serving corpus '{corpus}' at {} (Bus service '{bus_service}')",
        root.display()
    );
    tokio::select! {
        r = run_bus_loop(tx, client_tx, connected_tx, corpus, bus_service) => r,
        _ = shutdown() => { eprintln!("cosmix-filesd: shutdown"); Ok(()) }
    }
}

// ── The single index-writer ────────────────────────────────────────────────────

/// Work for the one Store-owning thread.
enum WriterCmd {
    /// A debounced watcher burst or a periodic tick — re-run the reconcile pass.
    Reconcile,
    /// An inbound Bus `filesd.*` command; reply (rc, json body) via the oneshot.
    Bus {
        cmd: IncomingCommand,
        reply: oneshot::Sender<(u8, String)>,
    },
    /// Re-push the whole live corpus to indexd (on each broker (re)connect) so a
    /// just-arrived / restarted indexd, or pushes dropped while the broker was down,
    /// converge. Bounded by corpus size; indexd dedups.
    ResyncIndexd,
}

/// An outbound semantic-index job for the pusher: index `key` with `content`, or
/// PURGE `key` when `content` is empty (indexd treats `content:""` as a delete).
struct PushJob {
    key: String,
    content: String,
}

struct Writer {
    root: PathBuf,
    corpus_id: String,
    started_at: String,
    started: Instant,
    store: Store,
    push_tx: mpsc::Sender<PushJob>,
    /// Keys abandoned or rejected by the pusher. Bodies are reconstructed from
    /// durable corpus state on the next reconcile, keeping retry debt small.
    failed_pushes: Arc<StdMutex<BTreeSet<String>>>,
    /// modseq up to which indexd pushes have entered the bounded queue. A failed
    /// post-cursor job is retained in `failed_pushes` and automatically rebuilt
    /// on the next reconcile; process restart resets this cursor to zero and
    /// replays the durable change stream.
    last_pushed: i64,
    /// Broker connectivity gate for the push cursor (set by the Bus loop).
    connected: tokio::sync::watch::Receiver<bool>,
    /// Peers allowed to use the `$cosmix_delegation` front-door (default `webd`).
    delegated_peers: Vec<String>,
}

impl Writer {
    fn run(mut self, mut rx: mpsc::Receiver<WriterCmd>) {
        self.reconcile_guarded(); // cold start / catch-up
        while let Some(cmd) = rx.blocking_recv() {
            match cmd {
                WriterCmd::Reconcile => {
                    self.reconcile_guarded();
                    self.retry_failed_pushes();
                    self.push_incremental();
                }
                WriterCmd::ResyncIndexd => {
                    // On (re)connect: first replay the change tail since `last_pushed`
                    // (recovers index+purge jobs that occurred while disconnected),
                    // then re-push the whole live set (recovers a wiped/fresh indexd).
                    self.retry_failed_pushes();
                    self.push_incremental();
                    self.resync_indexd(false);
                }
                WriterCmd::Bus { cmd, reply } => {
                    // Isolate a verb panic so it answers rc 10 instead of unwinding the
                    // writer thread (which would silently brick the whole daemon — the
                    // process keeps running but never indexes or replies again).
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        self.dispatch(&cmd)
                    }))
                    .unwrap_or_else(|_| (10, jerr("internal error handling command")));
                    let _ = reply.send(r);
                    self.push_incremental(); // pushes a save/move/delete's change(s); no-op for reads
                }
            }
        }
    }

    /// Enqueue indexd pushes for every change since `last_pushed` (the change stream
    /// carries `rel_path`/`old_rel_path`, so external watcher/reconcile edits and the
    /// explicit verbs flow through one path), then advance the cursor. Jobs that fail
    /// after enqueue remain represented by key in `failed_pushes` and are rebuilt from
    /// disk on the next reconcile. No-op when the
    /// broker is disconnected — the cursor stays put so these changes replay on
    /// reconnect rather than being dropped by the (clientless) pusher.
    fn push_incremental(&mut self) {
        if !*self.connected.borrow() {
            return;
        }
        let rows = match self.store.changes_since(self.last_pushed, 10_000) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("cosmix-filesd: changes_since for push failed: {e}");
                return;
            }
        };
        let Some(max) = rows.last().map(|c| c.modseq) else {
            return;
        };
        for c in &rows {
            match c.kind.as_str() {
                "REMOVED" => self.enqueue_purge(&c.rel_path),
                "RENAMED" => {
                    if let Some(old) = c.old_rel_path.as_deref() {
                        self.enqueue_purge(old);
                    }
                    self.enqueue_index(&c.rel_path);
                }
                _ => self.enqueue_index(&c.rel_path), // ADDED | CHANGED
            }
        }
        self.last_pushed = max;
    }

    fn retry_failed_pushes(&self) {
        let keys = {
            let mut failed = self
                .failed_pushes
                .lock()
                .expect("failed-push lock poisoned");
            std::mem::take(&mut *failed)
        };
        for key in keys {
            let rel_path = key
                .strip_prefix(&format!("{}/", self.corpus_id))
                .unwrap_or(&key);
            self.enqueue_current_state(rel_path);
        }
    }

    /// Re-push the entire current live corpus (idempotent; indexd dedups) — recovers a
    /// fresh/wiped indexd. Does NOT touch `last_pushed` (the delta cursor is owned by
    /// `push_incremental`, which runs first on reconnect to replay precise index+purge
    /// deltas). No-op when disconnected.
    ///
    /// When `purge_tombstones` is set (the operator `filesd.resync` path), it also
    /// purges every tombstoned path — the recovery for the one residual gap the delta
    /// cursor can't cover: a delete whose purge was dropped during an indexd outage
    /// while the broker stayed up (the cursor advanced, so no replay fires). The
    /// connect-time resync skips this (the change-tail replay already covers
    /// broker-down deletes, and it keeps reconnect cheap).
    fn resync_indexd(&mut self, purge_tombstones: bool) {
        if !*self.connected.borrow() {
            return;
        }
        match self.store.live_entries() {
            Ok(entries) => {
                for e in &entries {
                    self.enqueue_index(&e.rel_path);
                }
            }
            Err(e) => eprintln!("cosmix-filesd: resync live_entries failed: {e}"),
        }
        if purge_tombstones {
            match self.store.tombstoned_paths() {
                Ok(paths) => {
                    for p in &paths {
                        self.enqueue_purge(p);
                    }
                }
                Err(e) => eprintln!("cosmix-filesd: resync tombstoned_paths failed: {e}"),
            }
        }
    }

    fn enqueue_index(&self, rel_path: &str) {
        // Read current durable state. A read/delete race becomes a purge, which
        // is idempotent and avoids advancing the cursor past a missing index job.
        if let Ok(bytes) = std::fs::read(self.root.join(rel_path)) {
            self.enqueue_job(PushJob {
                key: self.index_key(rel_path),
                content: String::from_utf8_lossy(&bytes).into_owned(),
            });
        } else {
            self.enqueue_purge(rel_path);
        }
    }

    fn enqueue_purge(&self, rel_path: &str) {
        self.enqueue_job(PushJob {
            key: self.index_key(rel_path),
            content: String::new(),
        });
    }

    fn enqueue_current_state(&self, rel_path: &str) {
        self.enqueue_index(rel_path);
    }

    fn enqueue_job(&self, job: PushJob) {
        let key = job.key.clone();
        match self.push_tx.try_send(job) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                eprintln!("cosmix-filesd: indexd push queue full; retaining retry key {key}");
                self.failed_pushes
                    .lock()
                    .expect("failed-push lock poisoned")
                    .insert(key);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                eprintln!("cosmix-filesd: indexd pusher down; retaining retry key {key}");
                self.failed_pushes
                    .lock()
                    .expect("failed-push lock poisoned")
                    .insert(key);
            }
        }
    }

    /// The globally-unique indexd `$.path` key for a corpus-relative path.
    fn index_key(&self, rel_path: &str) -> String {
        format!("{}/{}", self.corpus_id, rel_path)
    }

    /// Run a reconcile pass, surviving both errors and panics (a panicked reconcile
    /// must not kill the single writer thread; rusqlite rolls back any open tx on the
    /// unwind).
    fn reconcile_guarded(&mut self) {
        let root = &self.root;
        let store = &mut self.store;
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            reconcile(root, store, now_ms())
        })) {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => eprintln!("cosmix-filesd: reconcile error: {e}"),
            Err(_) => eprintln!("cosmix-filesd: reconcile PANIC (recovered)"),
        }
    }

    /// Route one Bus command to its handler. Returns (rc, json body); rc 0 = ok,
    /// rc 10 = error `{"error":...}` (the contract a `send`/`bus_call` peer branches on).
    fn dispatch(&mut self, cmd: &IncomingCommand) -> (u8, String) {
        // S6a delegation gate (shared with fs-mode — see `gate`). A
        // `$cosmix_delegation` envelope selects the webd front-door path (fail closed,
        // never falls back); its absence is the bare trusted on-node path.
        let (deleg, args) = match gate(cmd, &self.delegated_peers) {
            Ok(v) => v,
            Err(refusal) => return refusal,
        };

        let verb = cmd.command.strip_prefix("filesd.").unwrap_or(&cmd.command);
        let (rc, body) = if let Some(suffix) = cmd.command.strip_prefix("filesd.props.") {
            self.props(suffix, &args)
        } else {
            match verb {
                "list" => self.v_list(&args),
                "read" => self.v_read(&args),
                "search" => self.v_search(&args),
                "changes" => self.v_changes(&args),
                "save" => self.v_save(&args),
                "move" => self.v_move(&args),
                "delete" => self.v_delete(&args),
                // Operator escape hatch: force a full re-push to indexd (e.g. after an
                // indexd DB wipe while the broker stayed up — which the connect-time
                // resync would otherwise miss).
                "resync" => {
                    self.resync_indexd(true); // thorough: re-index live + purge tombstones
                    (0, json!({ "ok": true }).to_string())
                }
                other => (10, jerr(&format!("unknown verb: filesd.{other}"))),
            }
        };
        if let Some(d) = &deleg {
            delegation::audit(&cmd.from, d, &cmd.command, rc);
        }
        (rc, body)
    }

    fn props(&self, suffix: &str, args: &serde_json::Value) -> (u8, String) {
        let stats = match self.store.corpus_stats() {
            Ok(s) => s,
            Err(e) => return (10, jerr(&e.to_string())),
        };
        let snap = props::FilesdProps {
            corpus_id: self.corpus_id.clone(),
            root: self.root.display().to_string(),
            started_at: self.started_at.clone(),
            uptime_s: self.started.elapsed().as_secs(),
            stats,
        };
        let a = if args.is_null() { None } else { Some(args) };
        let resp = cosmix_props_core::bus::dispatch_props(&snap, suffix, a, true);
        (resp.rc.clamp(0, 255) as u8, resp.body)
    }

    fn v_list(&self, args: &serde_json::Value) -> (u8, String) {
        let limit = args["limit"].as_i64().unwrap_or(200).clamp(1, 1000);
        let offset = args["offset"].as_i64().unwrap_or(0).max(0);
        match (self.store.list_docs(limit, offset), self.store.live_count()) {
            (Ok(rows), Ok(total)) => (0, json!({ "rows": rows, "total": total }).to_string()),
            (Err(e), _) | (_, Err(e)) => (10, jerr(&e.to_string())),
        }
    }

    fn v_read(&self, args: &serde_json::Value) -> (u8, String) {
        let found = if let Some(id) = args["id"].as_str() {
            self.store.get_doc_by_id(id)
        } else if let Some(p) = args["path"].as_str() {
            self.store.get_doc_by_path(p)
        } else {
            return (10, jerr("read: need \"id\" or \"path\""));
        };
        match found {
            Ok(Some(mut row)) => {
                if let Some(rel) = row["rel_path"].as_str().map(str::to_string) {
                    let full = self.root.join(&rel);
                    let too_big = std::fs::metadata(&full)
                        .map(|m| m.len() > MAX_READ_BODY)
                        .unwrap_or(false);
                    if too_big {
                        row["body_truncated"] = json!(true);
                    } else if let Ok(bytes) = std::fs::read(&full) {
                        row["body"] = json!(String::from_utf8_lossy(&bytes));
                    }
                }
                (0, json!({ "doc": row }).to_string())
            }
            Ok(None) => (10, jerr("not found")),
            Err(e) => (10, jerr(&e.to_string())),
        }
    }

    fn v_search(&self, args: &serde_json::Value) -> (u8, String) {
        let q = args["q"]
            .as_str()
            .or_else(|| args["query"].as_str())
            .unwrap_or("");
        if q.is_empty() {
            return (10, jerr("search: need \"q\""));
        }
        let limit = args["limit"].as_i64().unwrap_or(50).clamp(1, 1000);
        match self.store.search_docs(q, limit) {
            Ok(rows) => {
                let count = rows.len();
                (0, json!({ "rows": rows, "count": count }).to_string())
            }
            Err(e) => (10, jerr(&e.to_string())),
        }
    }

    fn v_changes(&self, args: &serde_json::Value) -> (u8, String) {
        // `since` accepts a string (the f64-safe wire form) or a number.
        let since = args["since"]
            .as_str()
            .and_then(|s| s.parse::<i64>().ok())
            .or_else(|| args["since"].as_i64())
            .unwrap_or(0);
        let limit = args["limit"].as_i64().unwrap_or(200).clamp(1, 1000);
        match self.store.changes_since(since, limit) {
            Ok(changes) => {
                let next = changes.last().map(|c| c.modseq.to_string());
                let arr: Vec<_> = changes
                    .iter()
                    .map(|c| {
                        json!({
                            "modseq": c.modseq.to_string(),
                            "doc_id": c.doc_id,
                            "kind": c.kind,
                            "changed_at": c.changed_at,
                        })
                    })
                    .collect();
                (0, json!({ "changes": arr, "next": next }).to_string())
            }
            Err(e) => (10, jerr(&e.to_string())),
        }
    }

    /// The ONLY id-minting write path (§6).
    fn v_save(&mut self, args: &serde_json::Value) -> (u8, String) {
        let rel = match args["path"].as_str() {
            Some(s) => s,
            None => return (10, jerr("save: need \"path\"")),
        };
        let full = match safe_full(&self.root, rel) {
            Some(f) => f,
            None => return (10, jerr("save: unsafe or empty \"path\"")),
        };
        let raw = args["content"].as_str().unwrap_or("");
        // Preserve identity on edit: if the new content is id-less but a doc already
        // lives at this path, adopt its existing id rather than minting a fresh one
        // (a web editor that posts only the body must not orphan the doc's links).
        let content: String = if frontmatter::get_field(raw, "id").is_none() {
            match std::fs::read_to_string(&full) {
                Ok(existing) => match frontmatter::get_field(&existing, "id") {
                    Some(eid) => frontmatter::set_field(raw, "id", eid.trim())
                        .unwrap_or_else(|_| raw.to_string()),
                    None => raw.to_string(),
                },
                Err(_) => raw.to_string(),
            }
        } else {
            raw.to_string()
        };
        let (text, id, _minted) = match frontmatter::ensure_id(&content) {
            Ok(v) => v,
            Err(e) => return (10, jerr(&format!("ensure_id: {e}"))),
        };
        if let Err(e) = ensure_parent(&full) {
            return (10, jerr(&format!("mkdir: {e}")));
        }
        if let Err(e) = atomic::write_atomic(&full, text.as_bytes()) {
            return (10, jerr(&format!("write_atomic: {e}")));
        }
        let rec = match ingest::scan(&self.root, rel) {
            Ok(r) => r,
            Err(e) => return (10, jerr(&format!("scan: {e}"))),
        };
        match self.store.upsert(&rec, now_ms()) {
            Ok((modseq, _)) => (
                0,
                json!({ "ok": true, "id": id, "path": rel, "modseq": modseq.to_string() })
                    .to_string(),
            ),
            Err(e) => (10, jerr(&format!("upsert: {e}"))),
        }
    }

    fn v_move(&mut self, args: &serde_json::Value) -> (u8, String) {
        let (from, from_full) = match args["from"]
            .as_str()
            .and_then(|s| safe_full(&self.root, s).map(|f| (s, f)))
        {
            Some(v) => v,
            None => return (10, jerr("move: bad \"from\"")),
        };
        let (to, to_full) = match args["to"]
            .as_str()
            .and_then(|s| safe_full(&self.root, s).map(|f| (s, f)))
        {
            Some(v) => v,
            None => return (10, jerr("move: bad \"to\"")),
        };
        if !from_full.try_exists().unwrap_or(false) {
            return (10, jerr("move: \"from\" does not exist"));
        }
        // Never clobber an existing destination (rename(2) silently replaces it).
        if to_full.try_exists().unwrap_or(true) {
            return (10, jerr("move: \"to\" already exists"));
        }
        if let Err(e) = ensure_parent(&to_full) {
            return (10, jerr(&format!("mkdir: {e}")));
        }
        if let Err(e) = std::fs::rename(&from_full, &to_full) {
            return (10, jerr(&format!("rename: {e}")));
        }
        let rec = match ingest::scan(&self.root, to) {
            Ok(r) => r,
            Err(e) => return (10, jerr(&format!("scan: {e}"))),
        };
        let now = now_ms();
        let modseq = match self.store.rename(from, to, &rec, now) {
            Ok(Some(m)) => m,
            // `from` wasn't indexed → treat the moved file as a fresh ingest.
            Ok(None) => match self.store.upsert(&rec, now) {
                Ok((m, _)) => m,
                Err(e) => return (10, jerr(&format!("upsert: {e}"))),
            },
            Err(e) => return (10, jerr(&format!("rename: {e}"))),
        };
        (
            0,
            json!({ "ok": true, "from": from, "to": to, "modseq": modseq.to_string() }).to_string(),
        )
    }

    fn v_delete(&mut self, args: &serde_json::Value) -> (u8, String) {
        let (rel, full) = match args["path"]
            .as_str()
            .and_then(|s| safe_full(&self.root, s).map(|f| (s, f)))
        {
            Some(v) => v,
            None => return (10, jerr("delete: bad \"path\"")),
        };
        // Remove the file (NotFound is fine — idempotent), then tombstone the row.
        match std::fs::remove_file(&full) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return (10, jerr(&format!("remove: {e}"))),
        }
        match self.store.tombstone(rel, now_ms()) {
            Ok(Some(m)) => (
                0,
                json!({ "ok": true, "path": rel, "modseq": m.to_string() }).to_string(),
            ),
            Ok(None) => (
                0,
                json!({ "ok": true, "path": rel, "modseq": null }).to_string(),
            ),
            Err(e) => (10, jerr(&e.to_string())),
        }
    }
}

// ── Producers (tokio tasks) ──────────────────────────────────────────────────────

fn spawn_watcher(root: PathBuf, tx: mpsc::Sender<WriterCmd>) -> anyhow::Result<()> {
    // A filesystem burst needs one reconcile signal, not one allocation per
    // kernel event. Capacity one coalesces the raw edge while the task sleeps;
    // reconcile itself remains lossless because it scans durable source state.
    let (raw_tx, mut raw_rx) = mpsc::channel::<()>(1);
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            use notify::EventKind;
            use notify::event::ModifyKind;
            let kind_relevant = matches!(
                ev.kind,
                EventKind::Create(_)
                    | EventKind::Remove(_)
                    | EventKind::Modify(ModifyKind::Data(_))
                    | EventKind::Modify(ModifyKind::Name(_))
                    | EventKind::Modify(ModifyKind::Any)
            );
            if kind_relevant && ev.paths.iter().any(|p| is_watch_relevant(p)) {
                let _ = raw_tx.try_send(());
            }
        }
    })?;
    watcher.watch(&root, notify::RecursiveMode::Recursive)?;
    tokio::spawn(async move {
        let _watcher = watcher; // keep the watcher (and its thread) alive for the task's life
        while raw_rx.recv().await.is_some() {
            tokio::time::sleep(DEBOUNCE).await; // let the burst settle
            while raw_rx.try_recv().is_ok() {} // drain it
            if tx.send(WriterCmd::Reconcile).await.is_err() {
                break; // writer gone
            }
        }
    });
    Ok(())
}

fn spawn_ticker(interval_secs: u64, tx: mpsc::Sender<WriterCmd>) {
    tokio::spawn(async move {
        let mut t = tokio::time::interval(Duration::from_secs(interval_secs.max(1)));
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        t.tick().await; // consume the immediate first tick (cold start already ran)
        loop {
            t.tick().await;
            if tx.send(WriterCmd::Reconcile).await.is_err() {
                break;
            }
        }
    });
}

// ── Bus citizen loop ─────────────────────────────────────────────────────────────

async fn run_bus_loop(
    tx: mpsc::Sender<WriterCmd>,
    client_tx: tokio::sync::watch::Sender<Option<Arc<NodedClient>>>,
    connected_tx: tokio::sync::watch::Sender<bool>,
    domain: String,
    service: String,
) -> anyhow::Result<()> {
    let bi = cosmix_buildinfo::build_info!();
    let prov = cosmix_bus::RegisterProvenance::from_parts(
        bi.pkg,
        bi.version,
        bi.git_sha,
        bi.git_dirty,
        bi.build_time,
        cosmix_buildinfo::now_rfc3339(),
    );
    let mut backoff = Duration::from_secs(1);
    loop {
        match cosmix_config::client_helpers::connect_default_with_provenance(&service, prov.clone())
            .await
        {
            Ok(client) => {
                backoff = Duration::from_secs(1);
                let client = Arc::new(client);
                // Canary: a synchronous probe proves the indexd source/metadata
                // contract before we trust fire-and-forget background pushes (which
                // ack before validating). Loud log on rejection.
                probe_indexd(&client, &domain).await;
                // Publish the live client + connectivity, and re-push the corpus to
                // indexd (replay the disconnected delta tail + recover a wiped indexd).
                let _ = client_tx.send_replace(Some(client.clone()));
                let _ = connected_tx.send_replace(true);
                if tx.send(WriterCmd::ResyncIndexd).await.is_err() {
                    return Err(anyhow::anyhow!("index writer down"));
                }
                serve_bus(client, &tx).await; // returns on disconnect
                let _ = connected_tx.send_replace(false);
                let _ = client_tx.send_replace(None);
            }
            Err(e) => eprintln!("cosmix-filesd: broker unavailable; retry in {backoff:?}: {e}"),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

/// One synchronous (`background:false`) index+purge of a throwaway path, to prove the
/// indexd source/metadata contract is accepted — background pushes ack before
/// validating, so without this canary a contract break is invisible to filesd.
async fn probe_indexd(client: &NodedClient, domain: &str) {
    let key = format!("{domain}/.filesd-probe");
    let index = json!({
        "path": key,
        "content": "cosmix-filesd indexd connectivity probe — confirms the source/metadata contract before fire-and-forget pushes.\n",
        "source": "doc",
        "domain": domain,
        "background": false,
    });
    match client.call("indexd", "indexd.index_file", index).await {
        Ok(v) if v.get("error").is_some() => {
            eprintln!("cosmix-filesd: indexd probe REJECTED (source/metadata contract): {v}");
        }
        Ok(_) => {
            // Purge the probe path (best-effort).
            let purge = json!({ "path": key, "content": "", "source": "doc", "domain": domain, "background": false });
            let _ = client.call("indexd", "indexd.index_file", purge).await;
        }
        Err(e) => {
            eprintln!("cosmix-filesd: indexd probe failed (indexd down or contract broken): {e}")
        }
    }
}

const INDEXD_PUSH_RETRY_CAP: Duration = Duration::from_secs(30 * 60);
const INDEXD_PUSH_MAX_BACKOFF: Duration = Duration::from_secs(60);

fn indexd_queue_full(message: &str) -> bool {
    message.contains("queue_full") || message.contains("background index queue")
}

fn next_push_backoff(current: Duration) -> Duration {
    (current * 2).min(INDEXD_PUSH_MAX_BACKOFF)
}

fn push_retry_remaining(elapsed: Duration) -> Option<Duration> {
    let remaining = INDEXD_PUSH_RETRY_CAP.saturating_sub(elapsed);
    (!remaining.is_zero()).then_some(remaining)
}

fn clipped_push_sleep(elapsed: Duration, backoff: Duration) -> Option<Duration> {
    push_retry_remaining(elapsed).map(|remaining| backoff.min(remaining))
}

/// Drain index jobs and push them to `cosmix-indexd` via the live broker client.
/// The current job remains owned by this serial loop until indexd accepts it.
/// queue_full and transport failures retry with bounded exponential backoff.
/// Every call and sleep is clipped to the same wall-clock deadline. Final
/// failure records only the key; the writer rebuilds the body from durable
/// corpus state on the next reconcile tick.
async fn run_pusher(
    mut rx: mpsc::Receiver<PushJob>,
    mut client_rx: tokio::sync::watch::Receiver<Option<Arc<NodedClient>>>,
    domain: String,
    failed_pushes: Arc<StdMutex<BTreeSet<String>>>,
) {
    while let Some(job) = rx.recv().await {
        let started = Instant::now();
        let mut backoff = Duration::from_secs(1);
        let mut accepted = false;
        'attempt: loop {
            let client = loop {
                if let Some(client) = client_rx.borrow().clone() {
                    break client;
                }
                let Some(remaining) = push_retry_remaining(started.elapsed()) else {
                    eprintln!(
                        "cosmix-filesd: indexd push ABANDONED after {:?} for {}: no live broker client",
                        INDEXD_PUSH_RETRY_CAP, job.key
                    );
                    break 'attempt;
                };
                match tokio::time::timeout(remaining, client_rx.changed()).await {
                    Ok(Ok(())) => {}
                    Ok(Err(_)) => return,
                    Err(_) => {
                        eprintln!(
                            "cosmix-filesd: indexd push ABANDONED after {:?} for {}: no live broker client",
                            INDEXD_PUSH_RETRY_CAP, job.key
                        );
                        break 'attempt;
                    }
                }
            };
            let args = json!({
                // "doc" is indexd's registered, dateless, generic document source;
                // the per-corpus discriminator is `domain` + the
                // `<corpus>/<rel>` path key.
                "path": job.key,
                "content": job.content,
                "source": "doc",
                "domain": domain,
                "background": true,
            });
            let Some(call_window) = push_retry_remaining(started.elapsed()) else {
                eprintln!(
                    "cosmix-filesd: indexd push ABANDONED after {:?} for {}: deadline exhausted before call",
                    INDEXD_PUSH_RETRY_CAP, job.key
                );
                break;
            };
            let retry_reason = match tokio::time::timeout(
                call_window,
                client.call_typed("indexd", "indexd.index_file", args),
            )
            .await
            {
                Ok(Ok(cosmix_bus::PortReply::Ok { .. })) => {
                    accepted = true;
                    break;
                }
                Ok(Ok(cosmix_bus::PortReply::AppError { message, .. }))
                    if indexd_queue_full(&message) =>
                {
                    format!("indexd queue_full: {message}")
                }
                Ok(Ok(cosmix_bus::PortReply::AppError { message, .. })) => {
                    eprintln!(
                        "cosmix-filesd: indexd push REJECTED for {} (not retryable): {message}",
                        job.key
                    );
                    break;
                }
                Ok(Err(e)) => format!("transport failure: {e}"),
                Err(_) => "indexd call exceeded remaining retry deadline".to_string(),
            };

            let Some(sleep_for) = clipped_push_sleep(started.elapsed(), backoff) else {
                eprintln!(
                    "cosmix-filesd: indexd push ABANDONED after {:?} for {}: {}",
                    INDEXD_PUSH_RETRY_CAP, job.key, retry_reason
                );
                break;
            };
            eprintln!(
                "cosmix-filesd: indexd push retry for {} in {:?}: {}",
                job.key, sleep_for, retry_reason
            );
            tokio::time::sleep(sleep_for).await;
            backoff = next_push_backoff(backoff);
        }
        if !accepted {
            failed_pushes
                .lock()
                .expect("failed-push lock poisoned")
                .insert(job.key);
        }
    }
}

/// The connection-owning command loop: forward each command to the writer with a
/// oneshot reply channel and emit the response itself (the client is re-created per
/// reconnect, so replies route through the loop, not the actor).
async fn serve_bus(client: Arc<NodedClient>, tx: &mpsc::Sender<WriterCmd>) {
    let Some(mut rx) = client.incoming_async().await else {
        return;
    };
    while let Some(cmd) = rx.recv().await {
        let (from, command, id) = (cmd.from.clone(), cmd.command.clone(), cmd.id.clone());
        let (reply_tx, reply_rx) = oneshot::channel();
        if tx
            .send(WriterCmd::Bus {
                cmd,
                reply: reply_tx,
            })
            .await
            .is_err()
        {
            let _ = client
                .respond_parts(
                    &from,
                    &command,
                    id.as_deref(),
                    10,
                    &jerr("index writer down"),
                )
                .await;
            break;
        }
        let (rc, body) = reply_rx.await.unwrap_or((10, jerr("writer dropped reply")));
        let _ = client
            .respond_parts(&from, &command, id.as_deref(), rc, &body)
            .await;
    }
}

// ── fs-mode: the generic file-manager capability ──────────────────────────────────
//
// A SEPARATE, lean serve path (D3-A): no Writer thread, no Store, no watcher, no
// indexd pusher — just the Bus loop + the shared delegation gate dispatching `fs.*`
// verbs against a stateless `FsLayer`. fs ops are independent syscall sequences, so
// commands are handled concurrently (the FS is the only shared state); each blocking
// op runs off the reactor via `spawn_blocking`.

async fn serve_fs(cfg: config::FsConfig) -> anyhow::Result<()> {
    let config::FsConfig {
        bus_service,
        places,
        trash_root,
        delegated_peers,
    } = cfg;
    let fs = Arc::new(cosmix_files::fsops::FsLayer::new(places, trash_root));
    let peers = Arc::new(delegated_peers);
    eprintln!("cosmix-filesd: serving file-manager fs layer (Bus service '{bus_service}')");
    tokio::select! {
        r = run_bus_loop_fs(fs, peers, bus_service) => r,
        _ = shutdown() => { eprintln!("cosmix-filesd: shutdown"); Ok(()) }
    }
}

async fn run_bus_loop_fs(
    fs: Arc<cosmix_files::fsops::FsLayer>,
    peers: Arc<Vec<String>>,
    service: String,
) -> anyhow::Result<()> {
    let bi = cosmix_buildinfo::build_info!();
    let prov = cosmix_bus::RegisterProvenance::from_parts(
        bi.pkg,
        bi.version,
        bi.git_sha,
        bi.git_dirty,
        bi.build_time,
        cosmix_buildinfo::now_rfc3339(),
    );
    let mut backoff = Duration::from_secs(1);
    loop {
        match cosmix_config::client_helpers::connect_default_with_provenance(&service, prov.clone())
            .await
        {
            Ok(client) => {
                backoff = Duration::from_secs(1);
                serve_bus_fs(Arc::new(client), &fs, &peers).await; // returns on disconnect
            }
            Err(e) => eprintln!("cosmix-filesd: broker unavailable; retry in {backoff:?}: {e}"),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

async fn serve_bus_fs(
    client: Arc<NodedClient>,
    fs: &Arc<cosmix_files::fsops::FsLayer>,
    peers: &Arc<Vec<String>>,
) {
    let Some(mut rx) = client.incoming_async().await else {
        return;
    };
    while let Some(cmd) = rx.recv().await {
        let (from, command, id) = (cmd.from.clone(), cmd.command.clone(), cmd.id.clone());
        let (rc, body) = dispatch_fs(cmd, fs.clone(), peers.clone()).await;
        let _ = client
            .respond_parts(&from, &command, id.as_deref(), rc, &body)
            .await;
    }
}

/// fs-mode dispatch: the shared delegation gate, then the (blocking) fs op off the
/// reactor via `spawn_blocking`.
async fn dispatch_fs(
    cmd: IncomingCommand,
    fs: Arc<cosmix_files::fsops::FsLayer>,
    peers: Arc<Vec<String>>,
) -> (u8, String) {
    let (deleg, args) = match gate(&cmd, &peers) {
        Ok(v) => v,
        Err(refusal) => return refusal,
    };
    let verb = cmd
        .command
        .strip_prefix("fs.")
        .unwrap_or(&cmd.command)
        .to_string();
    let (rc, body) = tokio::task::spawn_blocking(move || fs_verb(&fs, &verb, &args))
        .await
        .unwrap_or_else(|_| (10, jerr("internal error handling command")));
    if let Some(d) = &deleg {
        delegation::audit(&cmd.from, d, &cmd.command, rc);
    }
    (rc, body)
}

/// The synchronous fs-mode verb table. Any error (auth/scoping/io) becomes rc 10
/// `{"error":...}`. Irreversible ops (`delete`, `trash.empty`) require `confirm:true`
/// (the daemon's extra-confirm gate; the reversible default is `trash`).
fn fs_verb(fs: &cosmix_files::fsops::FsLayer, verb: &str, a: &serde_json::Value) -> (u8, String) {
    let r: std::result::Result<serde_json::Value, String> = (|| {
        Ok(match verb {
            "places" => fs.places(),
            "list" => fs
                .list(
                    req(a, "path")?,
                    a["show_hidden"].as_bool().unwrap_or(false),
                    a["sort"].as_str().unwrap_or("name"),
                    a["dir"].as_str().unwrap_or("asc"),
                )
                .map_err(estr)?,
            "stat" => fs.stat(req(a, "path")?).map_err(estr)?,
            "tree" => fs
                .tree(
                    req(a, "path")?,
                    // Clamp caller-supplied caps — a delegated caller must not be able
                    // to request an arbitrarily deep/large traversal.
                    (a["max_depth"].as_u64().unwrap_or(4) as usize).clamp(1, 8),
                    (a["max_nodes"].as_u64().unwrap_or(1000) as usize).clamp(1, 5000),
                )
                .map_err(estr)?,
            "read_blob" => fs
                .read_blob(req(a, "path")?, a["max"].as_u64().unwrap_or(1 << 20))
                .map_err(estr)?,
            "search" => fs
                .search(
                    req(a, "path")?,
                    req_query(a)?,
                    a["recursive"].as_bool().unwrap_or(true),
                    a["limit"].as_u64().unwrap_or(200) as usize,
                )
                .map_err(estr)?,
            "mkdir" => fs
                .mkdir(req(a, "path")?, a["parents"].as_bool().unwrap_or(false))
                .map_err(estr)?,
            "touch" => fs.touch(req(a, "path")?).map_err(estr)?,
            "write" => fs
                .write(
                    req(a, "path")?,
                    a["content"].as_str().unwrap_or("").as_bytes(),
                    a["overwrite"].as_bool().unwrap_or(false),
                )
                .map_err(estr)?,
            "copy" => fs
                .copy(
                    req(a, "from")?,
                    req(a, "to")?,
                    a["overwrite"].as_bool().unwrap_or(false),
                )
                .map_err(estr)?,
            "move" => fs
                .move_(
                    req(a, "from")?,
                    req(a, "to")?,
                    a["overwrite"].as_bool().unwrap_or(false),
                )
                .map_err(estr)?,
            "trash" => fs.trash(req(a, "path")?).map_err(estr)?,
            "trash.list" => fs.trash_list().map_err(estr)?,
            "trash.restore" => fs.trash_restore(req(a, "token")?).map_err(estr)?,
            "trash.empty" => {
                confirm(a)?;
                fs.trash_empty().map_err(estr)?
            }
            "delete" => {
                confirm(a)?;
                fs.delete(req(a, "path")?, a["recursive"].as_bool().unwrap_or(false))
                    .map_err(estr)?
            }
            other => return Err(format!("unknown verb: fs.{other}")),
        })
    })();
    match r {
        Ok(v) => (0, v.to_string()),
        Err(e) => (10, jerr(&e)),
    }
}

/// Required non-empty string arg.
fn req<'a>(a: &'a serde_json::Value, k: &str) -> std::result::Result<&'a str, String> {
    a[k].as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("missing or empty arg: {k}"))
}
/// `query` (or its `q` alias), required non-empty.
fn req_query(a: &serde_json::Value) -> std::result::Result<&str, String> {
    a["query"]
        .as_str()
        .or_else(|| a["q"].as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "missing arg: query".to_string())
}
/// The irreversible-op confirm gate.
fn confirm(a: &serde_json::Value) -> std::result::Result<(), String> {
    if a["confirm"].as_bool() == Some(true) {
        Ok(())
    } else {
        Err("this irreversible op requires confirm:true".to_string())
    }
}
fn estr(e: cosmix_files::error::FilesError) -> String {
    e.to_string()
}

async fn shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────────

/// Parse a command's args: the `args` header (JSON) wins, then a non-null `cmd.args`,
/// then the raw body as JSON. `None` if none present.
fn resolve_args(cmd: &IncomingCommand) -> Option<serde_json::Value> {
    if let Some(h) = cmd.header("args")
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(h)
    {
        return Some(v);
    }
    if !cmd.args.is_null() {
        return Some(cmd.args.clone());
    }
    if !cmd.body.is_empty()
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&cmd.body)
    {
        return Some(v);
    }
    None
}

/// The S6a delegation gate, shared by corpus `dispatch` and fs-mode `dispatch_fs`.
/// Returns `Ok((envelope?, effective_args))` for an authorized call, or
/// `Err((rc, body))` refusal. A `$cosmix_delegation` envelope selects the webd
/// front-door (fail closed, never demoted); its absence is the bare trusted on-node
/// path. Both security-interesting refusals are logged.
#[allow(clippy::type_complexity)]
fn gate(
    cmd: &IncomingCommand,
    delegated_peers: &[String],
) -> Result<(Option<delegation::Delegation>, serde_json::Value), (u8, String)> {
    match delegation::detect(cmd) {
        Some((env, inner_args)) => {
            // DELEGATED — the peer must be allowlisted (and non-empty: belt-and-
            // suspenders so an anonymous empty `from` can never match) and the
            // envelope valid. Both refusals are LOGGED (the front-door's two most
            // security-interesting events).
            if cmd.from.is_empty() || !delegated_peers.iter().any(|p| p == &cmd.from) {
                eprintln!(
                    "cosmix-filesd: delegated REFUSED: peer={:?} not an allowlisted delegated peer (verb={})",
                    cmd.from, cmd.command
                );
                return Err((
                    10,
                    jerr("auth_denied: delegated call from a non-delegated peer"),
                ));
            }
            match delegation::parse_envelope(&env) {
                Ok(d) => Ok((Some(d), inner_args)),
                Err(e) => {
                    eprintln!(
                        "cosmix-filesd: delegated REFUSED: peer={:?} bad envelope: {e} (verb={})",
                        cmd.from, cmd.command
                    );
                    Err((10, jerr(&format!("auth_denied: {e}"))))
                }
            }
        }
        None => {
            // BARE — defence-in-depth: a delegated peer must present an envelope (it
            // can never be silently demoted to the trusted-reachability path).
            if delegated_peers.iter().any(|p| p == &cmd.from) {
                eprintln!(
                    "cosmix-filesd: REFUSED: delegated peer={:?} used the bare path (no envelope; verb={})",
                    cmd.from, cmd.command
                );
                return Err((
                    10,
                    jerr(
                        "auth_denied: a delegated peer must present a $cosmix_delegation envelope",
                    ),
                ));
            }
            Ok((None, resolve_args(cmd).unwrap_or(serde_json::Value::Null)))
        }
    }
}

/// Resolve a corpus-relative path to the absolute path used for IO, ONLY if it is
/// guaranteed to stay within `root` — defeating textual `..`/absolute escapes AND
/// symlink traversal. A corpus path is always a file under the root, so an empty rel
/// is rejected (`allow_empty = false`). This delegates to the SINGLE shared resolver
/// (`cosmix_files::fsops::resolve_within`) that the file-manager `fs.*` layer also
/// uses — so traversal safety has one audited implementation.
fn safe_full(root: &Path, rel: &str) -> Option<PathBuf> {
    cosmix_files::fsops::resolve_within(root, rel, false)
}

fn ensure_parent(full: &Path) -> std::io::Result<()> {
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn is_watch_relevant(p: &Path) -> bool {
    if p.components().any(
        |c| matches!(c, std::path::Component::Normal(s) if s.to_string_lossy().starts_with('.')),
    ) {
        return false;
    }
    match p.file_name().and_then(|n| n.to_str()) {
        Some(name) => name.ends_with(".md") || name.contains(".sync-conflict-"),
        None => false,
    }
}

fn jerr(msg: &str) -> String {
    json!({ "error": msg }).to_string()
}

fn print_report(r: &ReconcileReport) {
    eprintln!(
        "cosmix-filesd reconcile: +{} ~{} -{} ->{} dup{} unmanaged{} conflicts{} errors{}",
        r.added,
        r.changed,
        r.removed,
        r.renamed,
        r.duplicates,
        r.unmanaged,
        r.conflicts,
        r.errors.len()
    );
    for (path, err) in &r.errors {
        eprintln!("  ingest error {path}: {err}");
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn scratch() -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!(
            "cosmix_filesd_t_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn writer_with_push(root: &Path) -> (Writer, mpsc::Receiver<PushJob>) {
        let (push_tx, push_rx) = mpsc::channel(64);
        // Tests run "connected" (true) so push_incremental/resync exercise the push
        // path; the sender is dropped but watch::Receiver::borrow keeps the last value.
        let (_conn_tx, connected) = tokio::sync::watch::channel(true);
        let w = Writer {
            root: root.to_path_buf(),
            corpus_id: "notes".to_string(),
            started_at: "1970-01-01T00:00:00Z".to_string(),
            started: Instant::now(),
            store: Store::open_in_memory("notes").unwrap(),
            push_tx,
            failed_pushes: Arc::new(StdMutex::new(BTreeSet::new())),
            last_pushed: 0,
            connected,
            delegated_peers: vec!["webd".to_string()],
        };
        (w, push_rx)
    }

    fn writer(root: &Path) -> Writer {
        writer_with_push(root).0 // push_rx dropped; dispatch tests don't push
    }

    fn cmd(command: &str, args: Value) -> IncomingCommand {
        IncomingCommand {
            from: "test".to_string(),
            command: command.to_string(),
            id: None,
            args,
            body: String::new(),
            headers: BTreeMap::new(),
        }
    }

    fn ok(w: &mut Writer, command: &str, args: Value) -> Value {
        let (rc, body) = w.dispatch(&cmd(command, args));
        assert_eq!(rc, 0, "expected ok for {command}, got {body}");
        serde_json::from_str(&body).unwrap()
    }

    // ── S6a delegation helpers + tests ──────────────────────────────────────
    fn cmd_from(from: &str, command: &str, args: Value) -> IncomingCommand {
        let mut c = cmd(command, args);
        c.from = from.to_string();
        c
    }

    fn good_envelope() -> Value {
        json!({
            "version": 1, "actor": "admin@example.org", "vhost": "example.org",
            "route_id": "corpus-notes", "role": "admin", "csrf_verified": true,
            "request_id": "req-1"
        })
    }

    /// A delegated command: body = `{$cosmix_delegation: env, args: inner}`.
    fn dcmd(from: &str, command: &str, env: Value, inner: Value) -> IncomingCommand {
        IncomingCommand {
            from: from.to_string(),
            command: command.to_string(),
            id: None,
            args: Value::Null,
            body: json!({ "$cosmix_delegation": env, "args": inner }).to_string(),
            headers: BTreeMap::new(),
        }
    }

    #[test]
    fn delegated_save_from_webd_authorizes_and_uses_inner_args() {
        let dir = scratch();
        let mut w = writer(&dir);
        let (rc, body) = w.dispatch(&dcmd(
            "webd",
            "filesd.save",
            good_envelope(),
            json!({ "path": "n.md", "content": "---\ntitle: X\n---\nbody\n" }),
        ));
        assert_eq!(rc, 0, "delegated save should authorize: {body}");
        assert!(
            dir.join("n.md").exists(),
            "the inner args drove a real write"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn delegated_read_from_webd_authorizes() {
        let dir = scratch();
        let mut w = writer(&dir);
        // Seed a doc via the bare trusted path, then read it back via delegation.
        ok(
            &mut w,
            "filesd.save",
            json!({ "path": "n.md", "content": "# A\n" }),
        );
        let (rc, body) = w.dispatch(&dcmd(
            "webd",
            "filesd.list",
            good_envelope(),
            json!({ "limit": 10 }),
        ));
        assert_eq!(rc, 0, "delegated read should authorize: {body}");
        let v: Value = serde_json::from_str(&body).unwrap();
        assert!(
            v["total"].as_i64().unwrap() >= 1,
            "the inner args drove a real list"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn delegated_envelope_from_non_webd_peer_fails_closed() {
        let dir = scratch();
        let mut w = writer(&dir);
        // A valid-looking envelope from a peer that is NOT allowlisted must be
        // refused — never authorized, never demoted to the bare trusted path.
        let (rc, body) = w.dispatch(&dcmd(
            "attacker",
            "filesd.save",
            good_envelope(),
            json!({ "path": "n.md", "content": "x" }),
        ));
        assert_eq!(rc, 10);
        assert!(body.contains("non-delegated peer"), "{body}");
        assert!(!dir.join("n.md").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn delegated_peer_without_envelope_is_refused() {
        let dir = scratch();
        let mut w = writer(&dir);
        // from=webd but a bare body (no envelope) → defence-in-depth refusal.
        let (rc, body) = w.dispatch(&cmd_from("webd", "filesd.list", Value::Null));
        assert_eq!(rc, 10);
        assert!(
            body.contains("must present a $cosmix_delegation envelope"),
            "{body}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn delegated_bad_envelope_refused() {
        let dir = scratch();
        let mut w = writer(&dir);
        // wrong role
        let mut env = good_envelope();
        env["role"] = json!("user");
        let (rc, _) = w.dispatch(&dcmd(
            "webd",
            "filesd.save",
            env,
            json!({ "path": "n.md", "content": "x" }),
        ));
        assert_eq!(rc, 10);
        assert!(!dir.join("n.md").exists());
        // malformed (non-object envelope) still selected the delegated path → refused.
        let (rc2, _) = w.dispatch(&dcmd("webd", "filesd.list", json!("nope"), json!({})));
        assert_eq!(rc2, 10);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bare_trusted_peer_unaffected_by_delegation_gate() {
        let dir = scratch();
        let mut w = writer(&dir);
        // from="test" is not a delegated peer + no envelope → the existing bare
        // on-node path is unchanged.
        let v = ok(
            &mut w,
            "filesd.save",
            json!({ "path": "n.md", "content": "# A\n" }),
        );
        assert!(v["id"].is_string());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_mints_id_writes_file_and_indexes() {
        let dir = scratch();
        let mut w = writer(&dir);
        let v = ok(
            &mut w,
            "filesd.save",
            json!({"path": "a.md", "content": "# Hi\n\nbody\n"}),
        );
        assert_eq!(v["ok"], true);
        let id = v["id"].as_str().unwrap().to_string();
        assert!(cosmix_files::id::is_valid_id(&id));
        assert!(v["modseq"].is_string()); // f64-safe wire form
        // The id was minted INTO the file on disk.
        let on_disk = std::fs::read_to_string(dir.join("a.md")).unwrap();
        assert!(on_disk.contains(&format!("id: {id}")));
        assert!(on_disk.contains("# Hi"));
        // And the index now lists it.
        let list = ok(&mut w, "filesd.list", json!({}));
        assert_eq!(list["total"], 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_returns_metadata_and_body() {
        let dir = scratch();
        let mut w = writer(&dir);
        let saved = ok(
            &mut w,
            "filesd.save",
            json!({"path": "n.md", "content": "---\ntitle: T\n---\nhello world\n"}),
        );
        let id = saved["id"].as_str().unwrap().to_string();
        let by_id = ok(&mut w, "filesd.read", json!({"id": id}));
        assert_eq!(by_id["doc"]["rel_path"], "n.md");
        assert_eq!(by_id["doc"]["title"], "T");
        assert!(
            by_id["doc"]["body"]
                .as_str()
                .unwrap()
                .contains("hello world")
        );
        let by_path = ok(&mut w, "filesd.read", json!({"path": "n.md"}));
        assert_eq!(by_path["doc"]["title"], "T");
        // missing → rc 10
        assert_eq!(
            w.dispatch(&cmd("filesd.read", json!({"path": "nope.md"})))
                .0,
            10
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn search_and_changes_and_delete() {
        let dir = scratch();
        let mut w = writer(&dir);
        ok(
            &mut w,
            "filesd.save",
            json!({"path": "alpha.md", "content": "---\ntitle: Alpha\n---\nx\n"}),
        );
        ok(
            &mut w,
            "filesd.save",
            json!({"path": "beta.md", "content": "---\ntitle: Beta\n---\ny\n"}),
        );
        // search
        let hits = ok(&mut w, "filesd.search", json!({"q": "alpha"}));
        assert_eq!(hits["count"], 1);
        assert_eq!(hits["rows"][0]["rel_path"], "alpha.md");
        // changes since 0 → two ADDED, with string modseqs + a string cursor
        let ch = ok(&mut w, "filesd.changes", json!({"since": "0"}));
        assert_eq!(ch["changes"].as_array().unwrap().len(), 2);
        assert!(ch["changes"][0]["modseq"].is_string());
        assert!(ch["next"].is_string());
        // delete tombstones → list drops to 1
        let del = ok(&mut w, "filesd.delete", json!({"path": "beta.md"}));
        assert_eq!(del["ok"], true);
        assert_eq!(ok(&mut w, "filesd.list", json!({}))["total"], 1);
        assert!(!dir.join("beta.md").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn move_renames_file_and_row() {
        let dir = scratch();
        let mut w = writer(&dir);
        let saved = ok(
            &mut w,
            "filesd.save",
            json!({"path": "from.md", "content": "---\ntitle: M\n---\nm\n"}),
        );
        let id = saved["id"].as_str().unwrap().to_string();
        let mv = ok(
            &mut w,
            "filesd.move",
            json!({"from": "from.md", "to": "sub/to.md"}),
        );
        assert_eq!(mv["ok"], true);
        assert!(!dir.join("from.md").exists());
        assert!(dir.join("sub/to.md").exists());
        // same id, now at the new path
        let read = ok(&mut w, "filesd.read", json!({"id": id}));
        assert_eq!(read["doc"]["rel_path"], "sub/to.md");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn props_get_reports_corpus_stats() {
        let dir = scratch();
        let mut w = writer(&dir);
        ok(
            &mut w,
            "filesd.save",
            json!({"path": "a.md", "content": "---\ntitle: A\n---\na\n"}),
        );
        // whole-tree get
        let all = ok(&mut w, "filesd.props.get", json!({}));
        // dispatch_props nests by path; corpus.files should be 1 somewhere in the tree.
        let body = serde_json::to_string(&all).unwrap();
        assert!(body.contains("\"files\""));
        // targeted get of the string modseq leaf
        let (rc, _b) = w.dispatch(&cmd("filesd.props.get", json!({"path": "corpus.modseq"})));
        assert_eq!(rc, 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unsafe_path_and_unknown_verb_rejected() {
        let dir = scratch();
        let mut w = writer(&dir);
        assert_eq!(
            w.dispatch(&cmd(
                "filesd.save",
                json!({"path": "../escape.md", "content": "x"})
            ))
            .0,
            10
        );
        assert_eq!(
            w.dispatch(&cmd(
                "filesd.save",
                json!({"path": "/abs.md", "content": "x"})
            ))
            .0,
            10
        );
        assert_eq!(w.dispatch(&cmd("filesd.bogus", json!({}))).0, 10);
        assert!(!Path::new("/abs.md").exists()); // never escaped the root
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn move_onto_existing_is_rejected_not_clobbering() {
        let dir = scratch();
        let mut w = writer(&dir);
        ok(
            &mut w,
            "filesd.save",
            json!({"path": "a.md", "content": "---\nid: 0190aaaa-bbbb-7ccc-8ddd-eeeeffff0001\n---\nA\n"}),
        );
        ok(
            &mut w,
            "filesd.save",
            json!({"path": "b.md", "content": "---\nid: 0190aaaa-bbbb-7ccc-8ddd-eeeeffff0002\n---\nB\n"}),
        );
        let (rc, _) = w.dispatch(&cmd("filesd.move", json!({"from": "a.md", "to": "b.md"})));
        assert_eq!(rc, 10); // refuse — would destroy b.md
        assert!(dir.join("a.md").exists() && dir.join("b.md").exists());
        // b.md is untouched (still B).
        assert!(
            std::fs::read_to_string(dir.join("b.md"))
                .unwrap()
                .contains("\nB\n")
        );
        // moving a nonexistent source is also refused.
        assert_eq!(
            w.dispatch(&cmd(
                "filesd.move",
                json!({"from": "nope.md", "to": "c.md"})
            ))
            .0,
            10
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_idless_over_existing_adopts_its_id() {
        let dir = scratch();
        let mut w = writer(&dir);
        let id = "0190aaaa-bbbb-7ccc-8ddd-eeeeffff0042";
        let first = ok(
            &mut w,
            "filesd.save",
            json!({"path": "n.md", "content": format!("---\nid: {id}\ntitle: One\n---\nv1\n")}),
        );
        assert_eq!(first["id"], id);
        // A subsequent id-less save (e.g. a body-only web edit) must keep the id.
        let second = ok(
            &mut w,
            "filesd.save",
            json!({"path": "n.md", "content": "just a new body, no frontmatter\n"}),
        );
        assert_eq!(
            second["id"], id,
            "id-less save over an existing doc must adopt its id"
        );
        assert!(
            std::fs::read_to_string(dir.join("n.md"))
                .unwrap()
                .contains(&format!("id: {id}"))
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn save_through_symlink_cannot_escape_root() {
        let dir = scratch();
        let outside = scratch();
        std::os::unix::fs::symlink(&outside, dir.join("link")).unwrap();
        let mut w = writer(&dir);
        let (rc, _) = w.dispatch(&cmd(
            "filesd.save",
            json!({"path": "link/evil.md", "content": "x"}),
        ));
        assert_eq!(rc, 10);
        assert!(!outside.join("evil.md").exists()); // the write never escaped the corpus root
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    #[test]
    fn push_incremental_and_resync_build_index_and_purge_jobs() {
        let dir = scratch();
        let (mut w, mut push_rx) = writer_with_push(&dir);
        // save (ADDED) → an index job; delete (REMOVED) → a purge job.
        ok(
            &mut w,
            "filesd.save",
            json!({"path": "a.md", "content": "# A\n"}),
        );
        w.push_incremental();
        ok(
            &mut w,
            "filesd.save",
            json!({"path": "b.md", "content": "# B\n"}),
        );
        ok(&mut w, "filesd.delete", json!({"path": "a.md"}));
        w.push_incremental();
        let mut jobs = Vec::new();
        while let Ok(j) = push_rx.try_recv() {
            jobs.push((j.key, j.content.is_empty()));
        }
        // index keys are corpus-namespaced; purge = empty content.
        assert!(
            jobs.contains(&("notes/a.md".to_string(), false)),
            "expected a.md index, got {jobs:?}"
        );
        assert!(
            jobs.contains(&("notes/b.md".to_string(), false)),
            "expected b.md index"
        );
        assert!(
            jobs.contains(&("notes/a.md".to_string(), true)),
            "expected a.md purge"
        );

        // Connect-time resync re-pushes the live set only (b.md — a.md is tombstoned).
        w.resync_indexd(false);
        let mut resync: Vec<_> = Vec::new();
        while let Ok(j) = push_rx.try_recv() {
            resync.push((j.key, j.content.is_empty()));
        }
        assert_eq!(resync, vec![("notes/b.md".to_string(), false)]);

        // Operator resync (purge_tombstones) ALSO purges the tombstoned a.md — the
        // recovery for a delete dropped during an indexd outage.
        w.resync_indexd(true);
        let mut thorough: Vec<_> = Vec::new();
        while let Ok(j) = push_rx.try_recv() {
            thorough.push((j.key, j.content.is_empty()));
        }
        assert!(
            thorough.contains(&("notes/b.md".to_string(), false)),
            "re-index live"
        );
        assert!(
            thorough.contains(&("notes/a.md".to_string(), true)),
            "purge tombstone"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn indexd_push_retry_classification_and_backoff_are_bounded() {
        assert!(indexd_queue_full("queue_full"));
        assert!(indexd_queue_full(
            "background index queue count/byte budget exhausted"
        ));
        assert!(!indexd_queue_full("validation failed: bad metadata"));
        assert_eq!(
            next_push_backoff(Duration::from_secs(1)),
            Duration::from_secs(2)
        );
        assert_eq!(
            next_push_backoff(Duration::from_secs(32)),
            Duration::from_secs(60)
        );
        assert_eq!(
            next_push_backoff(Duration::from_secs(60)),
            Duration::from_secs(60)
        );
        assert_eq!(
            clipped_push_sleep(
                INDEXD_PUSH_RETRY_CAP - Duration::from_secs(2),
                Duration::from_secs(60)
            ),
            Some(Duration::from_secs(2))
        );
        assert_eq!(
            push_retry_remaining(INDEXD_PUSH_RETRY_CAP - Duration::from_millis(250)),
            Some(Duration::from_millis(250)),
            "the following call is clipped to the same remaining window"
        );
        assert_eq!(
            clipped_push_sleep(INDEXD_PUSH_RETRY_CAP, Duration::from_secs(60)),
            None
        );
    }

    #[test]
    fn full_content_push_saturation_becomes_key_only_retry_debt() {
        let dir = scratch();
        let (mut w, _old_rx) = writer_with_push(&dir);
        let (tx, mut rx) = mpsc::channel(1);
        w.push_tx = tx;
        w.enqueue_job(PushJob {
            key: "notes/a.md".to_string(),
            content: "a".repeat(1024),
        });
        w.enqueue_job(PushJob {
            key: "notes/b.md".to_string(),
            content: "b".repeat(1024),
        });
        assert_eq!(
            w.failed_pushes
                .lock()
                .unwrap()
                .iter()
                .cloned()
                .collect::<Vec<_>>(),
            vec!["notes/b.md".to_string()],
            "saturated content is demoted to its durable retry key"
        );
        assert_eq!(rx.try_recv().unwrap().key, "notes/a.md");
        std::fs::write(dir.join("b.md"), "current durable body").unwrap();
        w.retry_failed_pushes();
        let retried = rx.try_recv().unwrap();
        assert_eq!(retried.key, "notes/b.md");
        assert_eq!(retried.content, "current durable body");
        assert!(w.failed_pushes.lock().unwrap().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn push_cursor_frozen_while_disconnected_then_replays_on_reconnect() {
        let dir = scratch();
        let (mut w, mut push_rx) = writer_with_push(&dir); // starts connected
        // Simulate broker DOWN.
        let (_down_tx, down) = tokio::sync::watch::channel(false);
        w.connected = down;
        ok(
            &mut w,
            "filesd.save",
            json!({"path": "a.md", "content": "# A\n"}),
        );
        w.push_incremental();
        assert!(
            push_rx.try_recv().is_err(),
            "disconnected → nothing enqueued"
        );
        assert_eq!(w.last_pushed, 0, "disconnected → cursor must NOT advance");
        // Broker back UP: the change tail must replay (incl. would-be purges).
        let (_up_tx, up) = tokio::sync::watch::channel(true);
        w.connected = up;
        w.push_incremental();
        let jobs: Vec<_> = std::iter::from_fn(|| push_rx.try_recv().ok())
            .map(|j| j.key)
            .collect();
        assert_eq!(jobs, vec!["notes/a.md".to_string()]);
        assert!(w.last_pushed > 0, "reconnected → cursor advances");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn failed_push_key_rebuilds_from_durable_state_on_next_reconcile() {
        let dir = scratch();
        let (w, mut push_rx) = writer_with_push(&dir);
        std::fs::write(dir.join("retry.md"), "first").unwrap();
        w.failed_pushes
            .lock()
            .unwrap()
            .insert("notes/retry.md".to_string());
        std::fs::write(dir.join("retry.md"), "current durable body").unwrap();

        w.retry_failed_pushes();
        let job = push_rx.try_recv().unwrap();
        assert_eq!(job.key, "notes/retry.md");
        assert_eq!(job.content, "current durable body");
        assert!(w.failed_pushes.lock().unwrap().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn push_rename_purges_old_key_and_indexes_new() {
        let dir = scratch();
        let (mut w, mut push_rx) = writer_with_push(&dir);
        ok(
            &mut w,
            "filesd.save",
            json!({"path": "from.md", "content": "---\nid: 0190aaaa-bbbb-7ccc-8ddd-eeeeffff0001\n---\nx\n"}),
        );
        w.push_incremental();
        ok(
            &mut w,
            "filesd.move",
            json!({"from": "from.md", "to": "to.md"}),
        );
        w.push_incremental();
        let mut jobs = Vec::new();
        while let Ok(j) = push_rx.try_recv() {
            jobs.push((j.key, j.content.is_empty()));
        }
        // rename → purge old key + index new key.
        assert!(
            jobs.contains(&("notes/from.md".to_string(), true)),
            "expected from.md purge, got {jobs:?}"
        );
        assert!(
            jobs.contains(&("notes/to.md".to_string(), false)),
            "expected to.md index"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_args_prefers_header_then_args_then_body() {
        let mut c = cmd("filesd.list", json!({"limit": 5}));
        assert_eq!(resolve_args(&c).unwrap()["limit"], 5);
        c.args = Value::Null;
        c.body = r#"{"limit": 9}"#.to_string();
        assert_eq!(resolve_args(&c).unwrap()["limit"], 9);
        c.headers
            .insert("args".to_string(), r#"{"limit": 1}"#.to_string());
        assert_eq!(resolve_args(&c).unwrap()["limit"], 1); // header wins
    }

    // ── fs-mode verb table ──────────────────────────────────────────────────────
    fn fs_layer(writable: bool) -> (cosmix_files::fsops::FsLayer, PathBuf) {
        use cosmix_files::fsops::{FsLayer, Place};
        let dir = scratch();
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let place = Place {
            id: "home".into(),
            label: "Home".into(),
            group: "places".into(),
            icon: "home".into(),
            root: home,
            writable,
            order: 0,
            allow: Vec::new(),
            deny: Vec::new(),
        };
        (FsLayer::new(vec![place], dir.join(".Trash")), dir)
    }

    #[test]
    fn fs_verb_places_list_and_unknown() {
        let (fs, dir) = fs_layer(true);
        std::fs::write(dir.join("home/a.txt"), b"hi").unwrap();
        let (rc, body) = fs_verb(&fs, "places", &json!({}));
        assert_eq!(rc, 0);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["places"][0]["id"], "home");
        let (rc, body) = fs_verb(&fs, "list", &json!({"path": "home"}));
        assert_eq!(rc, 0, "{body}");
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["file_count"], 1);
        // missing required arg + unknown verb → rc 10
        assert_eq!(fs_verb(&fs, "list", &json!({})).0, 10);
        assert_eq!(fs_verb(&fs, "bogus", &json!({})).0, 10);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fs_verb_irreversible_ops_need_confirm() {
        let (fs, dir) = fs_layer(true);
        std::fs::write(dir.join("home/x.txt"), b"x").unwrap();
        // delete without confirm → refused; the file survives.
        let (rc, body) = fs_verb(&fs, "delete", &json!({"path": "home/x.txt"}));
        assert_eq!(rc, 10);
        assert!(body.contains("confirm"), "{body}");
        assert!(dir.join("home/x.txt").exists());
        // with confirm → ok.
        assert_eq!(
            fs_verb(
                &fs,
                "delete",
                &json!({"path": "home/x.txt", "confirm": true})
            )
            .0,
            0
        );
        assert!(!dir.join("home/x.txt").exists());
        // trash.empty also needs confirm.
        assert_eq!(fs_verb(&fs, "trash.empty", &json!({})).0, 10);
        assert_eq!(fs_verb(&fs, "trash.empty", &json!({"confirm": true})).0, 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fs_verb_readonly_place_denies_write_verbs() {
        let (fs, dir) = fs_layer(false); // read-only
        std::fs::write(dir.join("home/r.txt"), b"r").unwrap();
        assert_eq!(fs_verb(&fs, "list", &json!({"path": "home"})).0, 0); // read ok
        assert_eq!(fs_verb(&fs, "mkdir", &json!({"path": "home/d"})).0, 10); // write denied
        assert_eq!(
            fs_verb(
                &fs,
                "delete",
                &json!({"path": "home/r.txt", "confirm": true})
            )
            .0,
            10
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn dispatch_fs_delegation_gate_applies() {
        let (fs, dir) = fs_layer(true);
        let fs = Arc::new(fs);
        let peers = Arc::new(vec!["webd".to_string()]);
        // A delegated peer with a valid admin envelope is authorized.
        let good = dcmd("webd", "fs.places", good_envelope(), json!({}));
        let (rc, _) = dispatch_fs(good, fs.clone(), peers.clone()).await;
        assert_eq!(rc, 0);
        // from=webd but no envelope → defence-in-depth refusal (the shared gate).
        let bare = cmd_from("webd", "fs.places", Value::Null);
        let (rc, body) = dispatch_fs(bare, fs.clone(), peers.clone()).await;
        assert_eq!(rc, 10);
        assert!(
            body.contains("must present a $cosmix_delegation envelope"),
            "{body}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
