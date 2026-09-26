//! Bus citizen for the `blob.*` namespace: metadata verbs, owner-tagged
//! pin events, GC sweep events and the props tree.
//!
//! Mesh-open per the 2026-09-15 law — no authorisation gates; quotas
//! are correctness, not authorisation. Bytes never ride a frame: this
//! namespace is metadata only; the byte lane (a later slice) moves
//! them. Every event publish carries `retain: false` — noded's
//! `topic.publish` defaults to `retain: true`, which would replay the
//! last event to any late subscriber.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use cosmix_bus::bus::BusMessage;
use cosmix_client::{IncomingCommand, NodedClient};
use cosmix_mds::blob::{self, PutMode};
use cosmix_mds::types::BlobHash;
use serde_json::{Value, json};
use tokio::sync::{Semaphore, mpsc};

use crate::core::reference::{self};
use crate::core::store::{PutOptions, Store, StoreError};
use crate::fetch::{FetchTarget, Fetcher, SubmitOutcome, TOPIC_FETCHED};
use crate::props::{BlobProps, PropsInput, props_diff_events};

pub const DEFAULT_SERVICE: &str = "blobd";
pub const TOPIC_PINNED: &str = "blob.pinned";
pub const TOPIC_UNPINNED: &str = "blob.unpinned";
pub const TOPIC_SWEPT: &str = "blob.swept";
pub const TOPIC_PROPS_CHANGED: &str = "blob.props.changed";

const BROKER_RECONNECT_DELAY: Duration = Duration::from_secs(60);
const OP_TIMEOUT: Duration = Duration::from_secs(60);

/// One `retain: false` publication queued by a dispatch or a fetch
/// completion.
pub struct BusEvent {
    pub(crate) topic: &'static str,
    pub(crate) message: BusMessage,
}

/// The dispatching citizen: a store plus its instance identity and
/// the fetch machinery.
pub struct Citizen {
    store: Arc<Store>,
    service: String,
    instance: String,
    lane_bind: Option<SocketAddr>,
    fetcher: Arc<Fetcher>,
    /// TEST ONLY (M2): per-verb artificial dispatch latency, slept at
    /// the top of `dispatch` (on the blocking pool) so the connection
    /// loop's concurrency test can hold one verb without a multi-GiB
    /// blob.
    #[cfg(test)]
    pub(crate) slow_verbs: BTreeMap<String, Duration>,
}

impl Citizen {
    pub fn new(
        store: Arc<Store>,
        service: String,
        instance: String,
        lane_bind: Option<SocketAddr>,
        fetcher: Arc<Fetcher>,
    ) -> Self {
        Self {
            store,
            service,
            instance,
            lane_bind,
            fetcher,
            #[cfg(test)]
            slow_verbs: BTreeMap::new(),
        }
    }

    /// Synchronous dispatch: Bus command in, `(rc, body, events)` out.
    /// Store work is blocking file/SQLite IO, so the connection loop
    /// runs this on the blocking pool.
    pub fn dispatch(&self, command: &IncomingCommand) -> (u8, String, Vec<BusEvent>) {
        #[cfg(test)]
        if let Some(delay) = self.slow_verbs.get(&command.command) {
            std::thread::sleep(*delay);
        }
        self.dispatch_inner(command)
            .unwrap_or_else(|e| (10, error_body(&e), Vec::new()))
    }

    fn dispatch_inner(
        &self,
        command: &IncomingCommand,
    ) -> Result<(u8, String, Vec<BusEvent>), StoreError> {
        let args = resolve_args(command);

        if let Some(suffix) = command.command.strip_prefix("blob.props.") {
            return Ok(self.props_dispatch(suffix, args.as_ref()));
        }

        match command.command.as_str() {
            "blob.put" => self.verb_put(command, args.as_ref()),
            "blob.stat" => self.verb_stat(args.as_ref()),
            "blob.path" => self.verb_path(args.as_ref()),
            "blob.url" => self.verb_url(args.as_ref()),
            "blob.has" => self.verb_has(args.as_ref()),
            "blob.pin" => self.verb_pin(command, args.as_ref(), true),
            "blob.unpin" => self.verb_pin(command, args.as_ref(), false),
            "blob.list" => self.verb_list(args.as_ref()),
            "blob.quota" => self.verb_quota(args.as_ref()),
            "blob.gc" => self.verb_gc(args.as_ref()),
            "blob.info" => Ok(self.verb_info()),
            "blob.fetch" => self.verb_fetch(command, args.as_ref()),
            other => Err(StoreError::BadRequest(format!(
                "unknown blob verb: {other}"
            ))),
        }
    }

    // ---- Verbs ----

    fn verb_put(
        &self,
        command: &IncomingCommand,
        args: Option<&Value>,
    ) -> Result<(u8, String, Vec<BusEvent>), StoreError> {
        let args =
            args.ok_or_else(|| StoreError::BadRequest("blob.put requires a JSON body".into()))?;
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| StoreError::BadRequest("blob.put requires a path".into()))?;
        let mode = match args.get("mode").and_then(Value::as_str) {
            None | Some("copy") => PutMode::Copy,
            Some("reflink") => PutMode::Reflink,
            Some("hardlink") => PutMode::HardLink,
            Some(other) => {
                return Err(StoreError::BadRequest(format!(
                    "blob.put mode {other:?} is not one of copy|reflink|hardlink"
                )));
            }
        };
        let owner = owner_from(command, args);
        let opts = PutOptions {
            owner: &owner,
            mime: args.get("mime").and_then(Value::as_str),
            name: args.get("name").and_then(Value::as_str),
            mode,
            immutable: args
                .get("immutable")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        };

        let before = self.props_input()?;
        let outcome = self.store.put(std::path::Path::new(path), &opts)?;
        let mut events = Vec::new();
        if outcome.newly_pinned {
            events.push(domain_event(
                TOPIC_PINNED,
                json!({"blob": reference::blob_id(&outcome.reference.hash), "owner": owner}),
            ));
        }
        self.append_props_events(before, &mut events);
        Ok((0, outcome.reference.to_json().to_string(), events))
    }

    fn verb_stat(&self, args: Option<&Value>) -> Result<(u8, String, Vec<BusEvent>), StoreError> {
        let hash = blob_arg(args)?;
        let stat = self.store.stat(&hash)?;
        let body = json!({
            "present": stat.present,
            "size": stat.size,
            "mime": stat.mime,
            "pins": stat.pins,
            "origin": stat.origin,
            "first_put": stat.first_put,
        });
        Ok((0, body.to_string(), Vec::new()))
    }

    fn verb_path(&self, args: Option<&Value>) -> Result<(u8, String, Vec<BusEvent>), StoreError> {
        let hash = blob_arg(args)?;
        let path = self.store.path(&hash)?;
        Ok((0, json!({"path": path}).to_string(), Vec::new()))
    }

    fn verb_url(&self, args: Option<&Value>) -> Result<(u8, String, Vec<BusEvent>), StoreError> {
        let hash = blob_arg(args)?;
        let lane = self.lane_bind.as_ref().ok_or_else(|| {
            StoreError::BadRequest("lane_bind is not configured; blob.url needs it".into())
        })?;
        // The URL is only truthful while this node holds the bytes.
        self.store.path(&hash)?;
        let url = format!("http://{lane}/blob/{}", blob::hex(&hash));
        Ok((0, json!({"url": url}).to_string(), Vec::new()))
    }

    fn verb_has(&self, args: Option<&Value>) -> Result<(u8, String, Vec<BusEvent>), StoreError> {
        let args =
            args.ok_or_else(|| StoreError::BadRequest("blob.has requires a JSON body".into()))?;
        let ids = args
            .get("blobs")
            .and_then(Value::as_array)
            .ok_or_else(|| StoreError::BadRequest("blob.has requires blobs: [ids]".into()))?;
        let mut hashes = Vec::with_capacity(ids.len());
        for id in ids {
            let id = id.as_str().ok_or_else(|| {
                StoreError::BadRequest("blob.has: every entry must be a blob id string".into())
            })?;
            hashes.push(parse_id(id)?);
        }
        let (present, missing) = self.store.has(&hashes)?;
        let body = json!({
            "present": present.iter().map(reference::blob_id).collect::<Vec<_>>(),
            "missing": missing.iter().map(reference::blob_id).collect::<Vec<_>>(),
        });
        Ok((0, body.to_string(), Vec::new()))
    }

    fn verb_pin(
        &self,
        command: &IncomingCommand,
        args: Option<&Value>,
        pin: bool,
    ) -> Result<(u8, String, Vec<BusEvent>), StoreError> {
        let args = args.ok_or_else(|| {
            StoreError::BadRequest(format!(
                "blob.{} requires a JSON body",
                if pin { "pin" } else { "unpin" }
            ))
        })?;
        let hash = blob_arg(Some(args))?;
        let owner = owner_from(command, args);

        let before = self.props_input()?;
        let changed = if pin {
            self.store.pin(&hash, &owner)?
        } else {
            self.store.unpin(&hash, &owner)?
        };
        let mut events = Vec::new();
        if changed {
            let (topic, event) = if pin {
                (TOPIC_PINNED, "blob.pinned")
            } else {
                (TOPIC_UNPINNED, "blob.unpinned")
            };
            events.push(domain_event(
                topic,
                json!({"event": event, "blob": reference::blob_id(&hash), "owner": owner}),
            ));
        }
        self.append_props_events(before, &mut events);
        Ok((
            0,
            json!({"blob": reference::blob_id(&hash), "owner": owner, "pinned": changed})
                .to_string(),
            events,
        ))
    }

    fn verb_list(&self, args: Option<&Value>) -> Result<(u8, String, Vec<BusEvent>), StoreError> {
        let owner = args.and_then(|a| a.get("owner")).and_then(Value::as_str);
        let limit = args
            .and_then(|a| a.get("limit"))
            .and_then(Value::as_u64)
            .unwrap_or(100) as usize;
        let cursor = match args.and_then(|a| a.get("cursor")).and_then(Value::as_str) {
            Some(c) => Some(parse_id(c)?),
            None => None,
        };
        let rows = self.store.list(owner, limit, cursor.as_ref())?;
        let mut blobs = Vec::with_capacity(rows.len());
        let mut next = None;
        for row in rows {
            next = Some(reference::blob_id(&row.hash));
            blobs.push(json!({
                "blob": reference::blob_id(&row.hash),
                "size": row.size,
                "mime": row.mime,
                "name": row.name,
                "origin": row.origin,
                "first_put": row.first_put,
                "pins": row.pins,
            }));
        }
        Ok((
            0,
            json!({"blobs": blobs, "next": next}).to_string(),
            Vec::new(),
        ))
    }

    fn verb_quota(&self, args: Option<&Value>) -> Result<(u8, String, Vec<BusEvent>), StoreError> {
        let owner = args.and_then(|a| a.get("owner")).and_then(Value::as_str);
        let report = self.store.quota_report(owner)?;
        let owners: serde_json::Map<String, Value> = report
            .owners
            .iter()
            .map(|(name, q)| {
                (
                    name.clone(),
                    json!({"used": q.used, "limit": q.limit, "reserved": q.reserved}),
                )
            })
            .collect();
        let body = json!({
            "owners": owners,
            "total": {
                "used": report.total.used,
                "limit": report.total.limit,
                "reserved": report.total.reserved,
            },
        });
        Ok((0, body.to_string(), Vec::new()))
    }

    fn verb_gc(&self, args: Option<&Value>) -> Result<(u8, String, Vec<BusEvent>), StoreError> {
        let dry_run = args
            .and_then(|a| a.get("dry_run"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let before = self.props_input()?;
        let sweep = self.store.gc(dry_run)?;
        let mut events = Vec::new();
        if !dry_run && !sweep.swept.is_empty() {
            events.push(domain_event(
                TOPIC_SWEPT,
                json!({"count": sweep.swept.len()}),
            ));
        }
        self.append_props_events(before, &mut events);
        let body = json!({
            "dry_run": dry_run,
            "count": sweep.swept.len(),
            "bytes_freed": sweep.bytes_freed,
            "swept": sweep.swept,
            "skipped": {
                "referenced": sweep.skipped_referenced,
                "pinned": sweep.skipped_pinned,
                "young": sweep.skipped_young,
            },
        });
        Ok((0, body.to_string(), events))
    }

    /// `blob.fetch {blob, from?, owner?}`: **replies immediately**,
    /// never a deferred reply (the 30 s mesh response timeout cannot
    /// carry a multi-GiB pull). Completion is the `blob.fetched`
    /// event plus the `blob.stat` transition. `blob` is a `b3:` id or
    /// a full reference (the reference's `origin` — and an `instance`
    /// member, if present — drive first-try resolution; `from`
    /// overrides the node).
    fn verb_fetch(
        &self,
        command: &IncomingCommand,
        args: Option<&Value>,
    ) -> Result<(u8, String, Vec<BusEvent>), StoreError> {
        let args =
            args.ok_or_else(|| StoreError::BadRequest("blob.fetch requires a JSON body".into()))?;
        let (hash, mut origin, mut instance) = match args.get("blob") {
            Some(Value::String(id)) => (parse_id(id)?, None, None),
            Some(value @ Value::Object(_)) => {
                let id = value.get("blob").and_then(Value::as_str).ok_or_else(|| {
                    StoreError::BadRequest("blob.fetch: a reference object needs a blob id".into())
                })?;
                let hash = parse_id(id)?;
                let origin = value
                    .get("origin")
                    .and_then(Value::as_str)
                    .map(String::from);
                let instance = value
                    .get("instance")
                    .and_then(Value::as_str)
                    .map(String::from);
                (hash, origin, instance)
            }
            _ => {
                return Err(StoreError::BadRequest(
                    "blob.fetch requires a blob id or a reference object".into(),
                ));
            }
        };
        if let Some(from) = args.get("from").and_then(Value::as_str) {
            origin = Some(from.to_string());
        }
        if let Some(name) = args.get("instance").and_then(Value::as_str) {
            instance = Some(name.to_string());
        }
        let owner = owner_from(command, args);
        let id = reference::blob_id(&hash);

        // If the CAS already has it, pin to the caller and done — the
        // reply is the completion.
        if matches!(blob::exists(&self.store.blobs_root(), &hash), Ok(true)) {
            let before = self.props_input()?;
            let changed = self.store.pin(&hash, &owner)?;
            let mut events = Vec::new();
            if changed {
                events.push(domain_event(
                    TOPIC_PINNED,
                    json!({"blob": id, "owner": owner}),
                ));
            }
            self.append_props_events(before, &mut events);
            let body = json!({
                "accepted": true,
                "blob": id,
                "origin": origin,
                "in_flight": false,
                "present": true,
            });
            return Ok((0, body.to_string(), events));
        }

        let target = origin.as_ref().map(|node| FetchTarget {
            node: node.clone(),
            instance: instance.clone(),
        });
        let before = self.props_input()?;
        let outcome = self.fetcher.submit(hash, target, owner);
        if outcome == SubmitOutcome::Busy {
            return Err(StoreError::Busy);
        }
        // in_flight: true when this call joined an existing fetch or
        // queued behind the concurrency bound; false when it started
        // the download itself.
        let in_flight = outcome == SubmitOutcome::Joined;
        let mut events = Vec::new();
        self.append_props_events(before, &mut events);
        let body = json!({
            "accepted": true,
            "blob": id,
            "origin": origin,
            "in_flight": in_flight,
            "present": false,
        });
        Ok((0, body.to_string(), events))
    }

    fn verb_info(&self) -> (u8, String, Vec<BusEvent>) {
        let build = cosmix_buildinfo::build_info!();
        let counts = self
            .store
            .counts()
            .map(|(blobs, pins)| json!({"blobs": blobs, "pins": pins}))
            .unwrap_or(Value::Null);
        let body = json!({
            "name": self.service,
            "schema": "blob.v1",
            "binary": build.pkg,
            "version": build.version,
            "git_sha": build.git_sha,
            "git_dirty": build.git_dirty,
            "build_time": build.build_time,
            "root": self.store.root(),
            "instance": self.instance,
            "lane_bind": self.lane_bind.map(|a| a.to_string()),
            "counts": counts,
        });
        (0, body.to_string(), Vec::new())
    }

    // ---- Props ----

    fn props_input(&self) -> Result<PropsInput, StoreError> {
        let (counts_blobs, counts_pins) = self.store.counts()?;
        let quota = self.store.quota_report(None)?;
        let gauges = self.fetcher.gauges();
        Ok(PropsInput {
            lane_bind: self
                .lane_bind
                .as_ref()
                .map(|a| a.to_string())
                .unwrap_or_default(),
            lane_port: self.lane_bind.map(|a| a.port()).unwrap_or(0),
            root: self.store.root().display().to_string(),
            instance: self.instance.clone(),
            counts_blobs,
            counts_pins,
            quota_total_used: quota.total.used,
            quota_total_limit: quota.total.limit,
            generation: self.store.generation(),
            fetch_in_flight: gauges.in_flight,
            fetch_queued: gauges.queued,
            fetch_completed: gauges.completed,
            fetch_failed: gauges.failed,
        })
    }

    fn props_dispatch(&self, suffix: &str, args: Option<&Value>) -> (u8, String, Vec<BusEvent>) {
        if suffix == "watch" {
            return (
                0,
                json!({
                    "topic": TOPIC_PROPS_CHANGED,
                    "domain_topics": [
                        TOPIC_PINNED,
                        TOPIC_UNPINNED,
                        TOPIC_SWEPT,
                        TOPIC_FETCHED,
                    ],
                    "generation": self.store.generation(),
                    "bootstrap": "subscribe on this connection, then read blob.props.get",
                })
                .to_string(),
                Vec::new(),
            );
        }
        match self.props_input() {
            Ok(input) => {
                let props = BlobProps::new(&input);
                let response = cosmix_props_core::bus::dispatch_props(&props, suffix, args, true);
                (response.rc.clamp(0, 255) as u8, response.body, Vec::new())
            }
            Err(e) => (10, error_body(&e), Vec::new()),
        }
    }

    /// Diff the props tree around a mutation into `blob.props.changed`
    /// events (transient leaves excluded by the shared differ).
    fn append_props_events(&self, before: PropsInput, events: &mut Vec<BusEvent>) {
        let after = match self.props_input() {
            Ok(input) => input,
            Err(_) => return,
        };
        events.extend(props_diff_events(&before, &after));
    }
}

// ---- Helpers ----

fn owner_from(command: &IncomingCommand, args: &Value) -> String {
    args.get("owner")
        .and_then(Value::as_str)
        .map(String::from)
        .unwrap_or_else(|| {
            if command.from.is_empty() {
                "unknown".into()
            } else {
                command.from.clone()
            }
        })
}

fn blob_arg(args: Option<&Value>) -> Result<BlobHash, StoreError> {
    let args = args
        .ok_or_else(|| StoreError::BadRequest("a JSON body with a blob id is required".into()))?;
    let id = args
        .get("blob")
        .and_then(Value::as_str)
        .ok_or_else(|| StoreError::BadRequest("a blob id is required".into()))?;
    parse_id(id)
}

fn parse_id(id: &str) -> Result<BlobHash, StoreError> {
    reference::parse_blob_id(id).ok_or_else(|| StoreError::InvalidBlob(id.to_string()))
}

fn error_body(e: &StoreError) -> String {
    json!({"error": e.to_string()}).to_string()
}

pub(crate) fn domain_event(topic: &'static str, body: Value) -> BusEvent {
    let mut message = BusMessage::new();
    message.set("command", topic);
    message.body = body.to_string();
    BusEvent { topic, message }
}

/// Arguments are resolved in this order: JSON from the Bus `args`
/// header, the command's non-null `args` value, then JSON parsed from
/// the raw body.
fn resolve_args(command: &IncomingCommand) -> Option<Value> {
    if let Some(args) = command.header("args")
        && let Ok(value) = serde_json::from_str(args)
    {
        return Some(value);
    }
    if !command.args.is_null() {
        return Some(command.args.clone());
    }
    if !command.body.is_empty()
        && let Ok(value) = serde_json::from_str(&command.body)
    {
        return Some(value);
    }
    None
}

/// `IncomingCommand` is not `Clone`; the blocking pool needs an owned
/// copy, and every field is.
fn clone_command(command: &IncomingCommand) -> IncomingCommand {
    IncomingCommand {
        from: command.from.clone(),
        command: command.command.clone(),
        id: command.id.clone(),
        args: command.args.clone(),
        body: command.body.clone(),
        headers: command.headers.clone(),
    }
}

// ---- Connection lifetime ----

/// Where a dispatched command's reply and its events go. Production
/// wraps the live [`NodedClient`]; the M2 test records reply
/// timestamps. Splitting this from the client is what makes the
/// connection loop's concurrency testable without a broker.
/// `BoxFuture` (the crate's shared `Send` boxed future), the same
/// idiom the fetch traits use.
trait ReplySink: Send + Sync + 'static {
    /// One Bus reply. An Err means the reply channel is gone; the task
    /// drops the reply (the incoming channel closing is what triggers
    /// the reconnect).
    fn respond<'a>(
        &'a self,
        command: &'a IncomingCommand,
        rc: u8,
        body: &'a str,
    ) -> crate::fetch::BoxFuture<'a, Result<()>>;
    /// One `retain: false` event publication. An Err is the caller's
    /// to log; publishing stays best-effort.
    fn publish<'a>(&'a self, event: &'a BusEvent) -> crate::fetch::BoxFuture<'a, Result<()>>;
}

struct ClientSink(Arc<NodedClient>);

impl ReplySink for ClientSink {
    fn respond<'a>(
        &'a self,
        command: &'a IncomingCommand,
        rc: u8,
        body: &'a str,
    ) -> crate::fetch::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.0
                .respond_parts(
                    &command.from,
                    &command.command,
                    command.id.as_deref(),
                    rc,
                    body,
                )
                .await
        })
    }

    fn publish<'a>(&'a self, event: &'a BusEvent) -> crate::fetch::BoxFuture<'a, Result<()>> {
        Box::pin(async move { publish_event(&self.0, event).await })
    }
}

/// Run the reconnecting citizen loop. Does not return during normal
/// operation; returns on SIGINT/SIGTERM. `verb_max_concurrent` bounds
/// how many dispatches run at once; a slow verb (a multi-GiB
/// `blob.put`, a CAS-walking `blob.gc`) holds one permit, not the
/// connection (M2).
pub async fn serve(citizen: Arc<Citizen>, verb_max_concurrent: usize) -> Result<()> {
    // The fetch dispatcher: one task per admitted download, living
    // across broker reconnects.
    citizen.fetcher.spawn_dispatcher().await;
    let build = cosmix_buildinfo::build_info!();
    let provenance = cosmix_bus::RegisterProvenance::from_parts(
        build.pkg,
        build.version,
        build.git_sha,
        build.git_dirty,
        build.build_time,
        cosmix_buildinfo::now_rfc3339(),
    );
    let service = citizen.service.clone();
    let permits = Arc::new(Semaphore::new(verb_max_concurrent.max(1)));

    loop {
        let connection = tokio::time::timeout(
            OP_TIMEOUT,
            cosmix_config::client_helpers::connect_default_with_provenance(
                &service,
                provenance.clone(),
            ),
        )
        .await;
        match connection {
            Ok(Ok(client)) => {
                let client = Arc::new(client);
                eprintln!("cosmix-blobd: registered as '{service}'");
                run_connection(&citizen, &client, &permits).await;
                client.close().await;
            }
            Ok(Err(error)) => {
                eprintln!("cosmix-blobd: broker unavailable; retrying in 60s: {error}");
            }
            Err(_) => {
                eprintln!("cosmix-blobd: broker connection timed out; retrying in 60s");
            }
        }
        tokio::select! {
            _ = shutdown_signal() => return Ok(()),
            _ = tokio::time::sleep(BROKER_RECONNECT_DELAY) => {}
        }
    }
}

async fn run_connection(citizen: &Arc<Citizen>, client: &Arc<NodedClient>, permits: &Arc<Semaphore>) {
    // The fetch machinery resolves and publishes through the live
    // connection; cleared when it ends, whenever it ends.
    citizen.fetcher.set_client(Some(Arc::clone(client)));
    run_connection_inner(citizen, client, permits).await;
    citizen.fetcher.set_client(None);
}

async fn run_connection_inner(
    citizen: &Arc<Citizen>,
    client: &Arc<NodedClient>,
    permits: &Arc<Semaphore>,
) {
    let Some(incoming) = client.incoming_async().await else {
        return;
    };
    serve_commands(
        Arc::clone(citizen),
        Arc::new(ClientSink(Arc::clone(client))),
        incoming,
        Arc::clone(permits),
    )
    .await;
}

/// Dispatch every incoming command as its own task, bounded by
/// `permits` (M2): the loop is only `recv` → spawn, so one slow verb
/// never holds the queue — `blob.fetch`'s early reply and every other
/// verb keep answering while a multi-GiB `blob.put` or a CAS walk
/// runs. Responding and publishing happen inside the task; no verb
/// needs cross-verb ordering (the store serialises what must be
/// serialised under its own locks). Returns when the incoming channel
/// closes, after draining the in-flight dispatches.
async fn serve_commands<S: ReplySink>(
    citizen: Arc<Citizen>,
    sink: Arc<S>,
    mut incoming: mpsc::UnboundedReceiver<IncomingCommand>,
    permits: Arc<Semaphore>,
) {
    let mut tasks = tokio::task::JoinSet::new();
    while let Some(command) = incoming.recv().await {
        let citizen = Arc::clone(&citizen);
        let sink = Arc::clone(&sink);
        let permits = Arc::clone(&permits);
        tasks.spawn(async move {
            // Beyond verb_max_concurrent the command waits here: its
            // reply is late, never lost.
            let _permit = permits.acquire_owned().await;
            let dispatch_command = clone_command(&command);
            let (rc, body, events) =
                tokio::task::spawn_blocking(move || citizen.dispatch(&dispatch_command))
                    .await
                    .unwrap_or_else(|panic| {
                        eprintln!("cosmix-blobd: dispatch panicked: {panic}");
                        (
                            10,
                            json!({"error": "internal dispatch failure"}).to_string(),
                            Vec::new(),
                        )
                    });
            if let Err(error) = sink.respond(&command, rc, &body).await {
                eprintln!("cosmix-blobd: Bus response failed; reply dropped: {error}");
                return;
            }
            for event in events {
                if let Err(error) = sink.publish(&event).await {
                    eprintln!(
                        "cosmix-blobd: publish on {} failed (continuing): {error}",
                        event.topic
                    );
                }
            }
        });
        // Reap what has finished so the set does not grow without
        // bound over a long-lived connection.
        while tasks.try_join_next().is_some() {}
    }
    // The channel closed: let the in-flight dispatches answer before
    // the connection is torn down.
    while tasks.join_next().await.is_some() {}
}

async fn publish_event(client: &Arc<NodedClient>, event: &BusEvent) -> Result<()> {
    let headers = BTreeMap::from([
        ("name".to_string(), event.topic.to_string()),
        // noded's topic.publish defaults retain: true; these events
        // must never replay to a late subscriber.
        ("retain".to_string(), "false".to_string()),
    ]);
    let wire = event.message.to_wire();
    tokio::time::timeout(
        OP_TIMEOUT,
        client.send_with_headers("noded", "topic.publish", &headers, &wire),
    )
    .await
    .context("publish timed out")?
    .context("publish failed")
}

#[cfg(unix)]
async fn shutdown_signal() -> Result<()> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("install SIGTERM handler")?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result.context("listen for SIGINT"),
        signal = terminate.recv() => signal
            .ok_or_else(|| anyhow!("SIGTERM stream ended"))
            .map(|_| ()),
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() -> Result<()> {
    tokio::signal::ctrl_c().await.context("listen for Ctrl-C")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::store::{StartupReport, StoreOptions};
    use crate::fetch::test_support::{NullPeers, NullResolver, TestSink};
    use crate::fetch::{FetchConfig, Fetcher};
    use serde_json::json;
    use std::time::Instant;
    use tempfile::TempDir;

    fn citizen() -> (TempDir, Citizen) {
        let dir = TempDir::new().unwrap();
        let store = Store::open(
            dir.path(),
            StoreOptions {
                origin: "testnode".into(),
                quota_total_bytes: crate::core::DEFAULT_QUOTA_TOTAL_BYTES,
                quota_owner_default_bytes: crate::core::DEFAULT_QUOTA_OWNER_BYTES,
                owner_limits: BTreeMap::new(),
            },
        )
        .unwrap();
        let store = Arc::new(store);
        let fetcher = Fetcher::new(
            Arc::clone(&store),
            FetchConfig::default(),
            Some("10.42.0.5:4210".parse().unwrap()),
            "default".into(),
            Arc::new(NullResolver),
            Arc::new(NullPeers),
            Arc::new(TestSink::default()),
        );
        (
            dir,
            Citizen::new(
                store,
                "blobd".into(),
                "default".into(),
                Some("10.42.0.5:4210".parse().unwrap()),
                Arc::new(fetcher),
            ),
        )
    }

    fn command(verb: &str, from: &str, args: Value) -> IncomingCommand {
        let mut headers = BTreeMap::new();
        if !args.is_null() {
            headers.insert("args".into(), args.to_string());
        }
        IncomingCommand {
            from: from.into(),
            command: verb.into(),
            id: Some("1".into()),
            args: Value::Null,
            body: String::new(),
            headers,
        }
    }

    #[test]
    fn put_stat_has_and_quota_dispatch() {
        let (dir, c) = citizen();
        let src = dir.path().join("dispatch.bin");
        std::fs::write(&src, b"dispatch me").unwrap();

        // put: pins to the calling service name, sniffs the mime.
        let (rc, body, events) = c.dispatch(&command("blob.put", "maild", json!({"path": src})));
        assert_eq!(rc, 0, "{body}");
        let reference: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(reference["mime"], "application/octet-stream");
        assert_eq!(reference["origin"], "testnode");
        assert_eq!(events.len(), 4, "pinned + 3 props.changed leaves");
        assert_eq!(events[0].topic, TOPIC_PINNED);
        assert!(events[1..].iter().all(|e| e.topic == TOPIC_PROPS_CHANGED));
        let id = reference["blob"].as_str().unwrap().to_string();

        // stat
        let (rc, body, _) = c.dispatch(&command("blob.stat", "maild", json!({"blob": id})));
        assert_eq!(rc, 0, "{body}");
        let stat: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(stat["present"], true);
        assert_eq!(stat["pins"], json!(["maild"]));

        // has
        let (rc, body, _) = c.dispatch(&command(
            "blob.has",
            "maild",
            json!({"blobs": [id, "b3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]}),
        ));
        assert_eq!(rc, 0, "{body}");
        let has: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(has["present"].as_array().unwrap().len(), 1);
        assert_eq!(has["missing"].as_array().unwrap().len(), 1);

        // quota
        let (rc, body, _) = c.dispatch(&command("blob.quota", "maild", json!({})));
        assert_eq!(rc, 0, "{body}");
        let quota: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(quota["owners"]["maild"]["used"], 11);
        assert_eq!(quota["total"]["used"], 11);
    }

    #[test]
    fn put_owner_override_and_mode_validation() {
        let (dir, c) = citizen();
        let src = dir.path().join("override.txt");
        std::fs::write(&src, b"override").unwrap();

        let (rc, body, _) = c.dispatch(&command(
            "blob.put",
            "filesd",
            json!({"path": src, "owner": "capture", "name": "shot.png"}),
        ));
        assert_eq!(rc, 0, "{body}");
        let reference: Value = serde_json::from_str(&body).unwrap();
        // name drives the mime sniff when mime is absent.
        assert_eq!(reference["mime"], "image/png");
        assert_eq!(reference["name"], "shot.png");

        let src2 = dir.path().join("hard.bin");
        std::fs::write(&src2, b"hard").unwrap();
        let (rc, body, _) = c.dispatch(&command(
            "blob.put",
            "filesd",
            json!({"path": src2, "mode": "hardlink"}),
        ));
        assert_eq!(rc, 10, "{body}");
        let err: Value = serde_json::from_str(&body).unwrap();
        assert!(err["error"].as_str().unwrap().contains("immutable"));

        let (rc, _, _) = c.dispatch(&command(
            "blob.put",
            "filesd",
            json!({"path": src2, "mode": "warp"}),
        ));
        assert_eq!(rc, 10);
        let (rc, _, _) = c.dispatch(&command("blob.put", "filesd", json!({})));
        assert_eq!(rc, 10);
    }

    #[test]
    fn stat_path_url_error_and_fetch_present() {
        let (dir, c) = citizen();
        let src = dir.path().join("u.bin");
        std::fs::write(&src, b"url me").unwrap();
        let (_, body, _) = c.dispatch(&command("blob.put", "t", json!({"path": src})));
        let id = serde_json::from_str::<Value>(&body).unwrap()["blob"]
            .as_str()
            .unwrap()
            .to_string();

        let (rc, body, _) = c.dispatch(&command("blob.url", "t", json!({"blob": id})));
        assert_eq!(rc, 0, "{body}");
        let url: Value = serde_json::from_str(&body).unwrap();
        assert!(
            url["url"]
                .as_str()
                .unwrap()
                .starts_with("http://10.42.0.5:4210/blob/")
        );

        let (rc, _body, _) = c.dispatch(&command("blob.path", "t", json!({"blob": id})));
        assert_eq!(rc, 0);
        let missing = "b3:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let (rc, body, _) = c.dispatch(&command("blob.path", "t", json!({"blob": missing})));
        assert_eq!(rc, 10);
        assert!(body.contains("not_present"));

        // fetch of a present blob: the immediate reply is the
        // completion — pinned to the caller, no fetch started.
        let (rc, body, events) = c.dispatch(&command("blob.fetch", "maild", json!({"blob": id})));
        assert_eq!(rc, 0, "{body}");
        let reply: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(reply["accepted"], true);
        assert_eq!(reply["present"], true);
        assert_eq!(reply["in_flight"], false);
        assert_eq!(reply["origin"], Value::Null);
        assert_eq!(events[0].topic, TOPIC_PINNED);
        let pinned: Value = serde_json::from_str(&events[0].message.body).unwrap();
        assert_eq!(pinned["owner"], "maild");
        let stat = c
            .dispatch(&command("blob.stat", "maild", json!({"blob": id})))
            .1;
        assert!(
            serde_json::from_str::<Value>(&stat).unwrap()["pins"]
                .as_array()
                .unwrap()
                .iter()
                .any(|o| o == "maild")
        );

        let (rc, body, _) = c.dispatch(&command("blob.warp", "t", json!({})));
        assert_eq!(rc, 10);
        assert!(body.contains("unknown blob verb"));

        let (rc, body, _) = c.dispatch(&command("blob.stat", "t", json!({"blob": "nope"})));
        assert_eq!(rc, 10);
        assert!(body.contains("invalid blob id"));
    }

    #[test]
    fn fetch_arg_shapes() {
        let (dir, c) = citizen();
        // A reference object drives origin/instance; bad shapes refuse.
        let (rc, _, _) = c.dispatch(&command("blob.fetch", "t", json!({"blob": 7})));
        assert_eq!(rc, 10);
        let (rc, _, _) = c.dispatch(&command("blob.fetch", "t", json!({})));
        assert_eq!(rc, 10);
        let (rc, body, _) = c.dispatch(&command(
            "blob.fetch",
            "t",
            json!({"blob": {"blob": "b3:tooshort"}}),
        ));
        assert_eq!(rc, 10, "{body}");
        assert!(body.contains("invalid blob id"));
        // A valid id for bytes we do not hold is accepted (the fetch
        // itself goes nowhere in this runtime-less test — the reply
        // shape is what is asserted).
        let absent = "b3:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
        let (rc, body, _) = c.dispatch(&command(
            "blob.fetch",
            "maild",
            json!({"blob": {"blob": absent, "size": 5, "mime": "text/plain", "origin": "alpha"}}),
        ));
        assert_eq!(rc, 0, "{body}");
        let reply: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(reply["accepted"], true);
        assert_eq!(reply["present"], false);
        assert_eq!(reply["origin"], "alpha");
        let _ = dir;
    }

    #[test]
    fn props_dispatch_lists_and_gets() {
        let (_dir, c) = citizen();
        let (rc, body, _) = c.dispatch(&command("blob.props.list", "t", json!({})));
        assert_eq!(rc, 0, "{body}");
        let list: Value = serde_json::from_str(&body).unwrap();
        assert!(list.as_array().unwrap().iter().any(|p| p == "counts.blobs"));

        let (rc, body, _) = c.dispatch(&command("blob.props.get", "t", json!({})));
        assert_eq!(rc, 0, "{body}");
        let got: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(got["lane"]["port"], 4210);

        let (rc, body, _) = c.dispatch(&command(
            "blob.props.get",
            "t",
            json!({"path": "lane.port"}),
        ));
        assert_eq!(rc, 0, "{body}");
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap(), 4210);

        let (rc, body, _) = c.dispatch(&command("blob.props.watch", "t", json!({})));
        assert_eq!(rc, 0);
        assert!(body.contains(TOPIC_PROPS_CHANGED));
    }

    #[test]
    fn unpin_then_gc_dry_run_dispatch() {
        let (dir, c) = citizen();
        let src = dir.path().join("gc.bin");
        std::fs::write(&src, b"gc me").unwrap();
        let (_, body, _) = c.dispatch(&command("blob.put", "tmp", json!({"path": src})));
        let id = serde_json::from_str::<Value>(&body).unwrap()["blob"]
            .as_str()
            .unwrap()
            .to_string();

        let (rc, body, events) = c.dispatch(&command("blob.unpin", "tmp", json!({"blob": id})));
        assert_eq!(rc, 0, "{body}");
        assert_eq!(events[0].topic, TOPIC_UNPINNED);

        // Young file: dry run finds nothing to sweep.
        let (rc, body, _) = c.dispatch(&command("blob.gc", "t", json!({"dry_run": true})));
        assert_eq!(rc, 0, "{body}");
        let gc: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(gc["count"], 0);
    }

    #[test]
    fn events_carry_the_topic_command_and_json_body() {
        let (dir, c) = citizen();
        let src = dir.path().join("ev.bin");
        std::fs::write(&src, b"event").unwrap();
        let (_, body, events) = c.dispatch(&command("blob.put", "maild", json!({"path": src})));
        let id = serde_json::from_str::<Value>(&body).unwrap()["blob"]
            .as_str()
            .unwrap()
            .to_string();

        let pinned = &events[0];
        assert_eq!(pinned.topic, "blob.pinned");
        let parsed: Value = serde_json::from_str(&pinned.message.body).unwrap();
        assert_eq!(parsed["blob"], id.as_str());
        assert_eq!(parsed["owner"], "maild");
    }

    #[test]
    fn startup_report_shape() {
        // Sanity on the report the daemon logs at open.
        let report = StartupReport {
            tmp_removed: 2,
            orphans: vec!["abcd".into()],
        };
        assert_eq!(report.tmp_removed, 2);
        assert_eq!(report.orphans.len(), 1);
    }

    // ---- M2: one slow verb never blocks another ----

    /// Records each reply with the instant it landed.
    #[derive(Default)]
    struct RecordSink(std::sync::Mutex<Vec<(String, Instant)>>);

    impl ReplySink for RecordSink {
        fn respond<'a>(
            &'a self,
            command: &'a IncomingCommand,
            _rc: u8,
            _body: &'a str,
        ) -> crate::fetch::BoxFuture<'a, Result<()>> {
            self.0
                .lock()
                .unwrap()
                .push((command.command.clone(), Instant::now()));
            Box::pin(std::future::ready(Ok(())))
        }

        fn publish<'a>(&'a self, _event: &'a BusEvent) -> crate::fetch::BoxFuture<'a, Result<()>> {
            Box::pin(std::future::ready(Ok(())))
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_slow_verb_does_not_delay_a_concurrent_blob_info() {
        let (dir, mut c) = citizen();
        // A 2 s blob.put (the slow-verb hook stands in for a multi-GiB
        // hash+copy+re-hash) dispatched first; blob.info arriving right
        // behind it must still answer within 200 ms.
        c.slow_verbs
            .insert("blob.put".into(), Duration::from_secs(2));
        let citizen = Arc::new(c);
        let sink = Arc::new(RecordSink::default());
        let (tx, rx) = mpsc::unbounded_channel();
        let start = Instant::now();
        tx.send(command(
            "blob.put",
            "maild",
            json!({"path": dir.path().join("slow.bin")}),
        ))
        .unwrap();
        tx.send(command("blob.info", "maild", json!({}))).unwrap();
        drop(tx);
        serve_commands(
            Arc::clone(&citizen),
            Arc::clone(&sink),
            rx,
            Arc::new(Semaphore::new(8)),
        )
        .await;
        let replies = sink.0.lock().unwrap().clone();
        assert_eq!(replies.len(), 2, "both commands answered");
        let (_, info_at) = replies
            .iter()
            .find(|(verb, _)| verb == "blob.info")
            .expect("blob.info replied");
        let (_, put_at) = replies
            .iter()
            .find(|(verb, _)| verb == "blob.put")
            .expect("blob.put replied");
        assert!(
            info_at.duration_since(start) < Duration::from_millis(200),
            "blob.info answered in {:?} while blob.put was still running",
            info_at.duration_since(start)
        );
        assert!(
            put_at.duration_since(*info_at) >= Duration::from_secs(2),
            "blob.put overlapped blob.info by {:?} — the slow verb was not held",
            put_at.duration_since(*info_at)
        );
    }
}
