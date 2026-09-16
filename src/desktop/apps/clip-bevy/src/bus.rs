//! All network work stays on the native Bus client, outside the UI thread.
use std::{collections::BTreeMap, io::Read, sync::Arc, time::Duration};

use bevy::prelude::Resource;
use cosmix_client::{BoundedIncomingEvent, BoundedIncomingReceiver, SupervisedClient};
use serde_json::{json, Value};
use tokio::time::{sleep_until, timeout, Instant};

type Result<T> = std::result::Result<T, String>;

pub struct Config {
    url: String,
    name: String,
    local: String,
    remote: String,
}

impl Config {
    fn load() -> Result<Self> {
        let node = std::fs::read_to_string("/etc/cosmix/node.conf.mix").unwrap_or_default();
        let capture = |source: &str, pattern: &str| {
            regex::Regex::new(pattern)
                .ok()?
                .captures(source)?
                .get(1)
                .map(|m| m.as_str().to_owned())
        };
        let host =
            capture(&node, r#""wg_ip"\s*:\s*"([^"]+)""#).unwrap_or_else(|| "127.0.0.1".into());
        let port = capture(&node, r#""noded"\s*:\s*\{[^}]*"port"\s*:\s*(\d+)"#)
            .unwrap_or_else(|| "4200".into());
        let targets = std::fs::read_to_string("/etc/cosmix/clipboard.conf.mix").unwrap_or_default();
        let mut nonce = [0u8; 10];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut nonce))
            .map_err(|e| e.to_string())?;
        let name = format!(
            "clippanel-{}",
            nonce.iter().map(|b| format!("{b:02x}")).collect::<String>()
        );
        Ok(Self {
            url: format!("ws://{host}:{port}/ws"),
            name,
            local: capture(&targets, r#"\blocal\s*:\s*"([^"]+)""#)
                .unwrap_or_else(|| "desktop-vt1".into()),
            remote: capture(&targets, r#"\bremote\s*:\s*"([^"]+)""#).unwrap_or_default(),
        })
    }
}

#[derive(Clone, Default)]
pub struct Snapshot {
    pub local: Value,
    pub remote: Value,
    pub entries: Vec<Value>,
    pub remote_entries: Vec<Value>,
    pub error: String,
}

pub enum Request {
    Search(String),
    Pick(Value),
    RemotePick(Value),
    Pause(bool),
    Clear,
    ClearTimer(u64),
}

pub enum Event {
    Snapshot(Box<Snapshot>),
    Toggle,
    ClearExpired(u64),
    Error(String),
}

#[derive(Resource)]
pub struct Bus {
    pub requests: flume::Sender<Request>,
    pub events: flume::Receiver<Event>,
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("clipboard runtime")
}

impl Bus {
    pub fn start(wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        let (requests, rx) = flume::bounded(32);
        let (tx, events) = flume::bounded(256);
        std::thread::spawn(move || {
            runtime().block_on(async {
                let result = async {
                    let config = Config::load()?;
                    let mut session = Session::connect(config).await?;
                    let mut incoming = session.client.incoming_bounded().ok_or("No incoming lane")?;
                    let mut connection = session.client.subscribe_state();
                    let mut due = Some(Instant::now() + Duration::from_millis(250));
                    let mut clear_due: Option<(Instant, u64)> = None;
                    loop {
                        let far = Instant::now() + Duration::from_secs(86400 * 365);
                        tokio::select! {
                            request = rx.recv_async() => {
                                let Ok(request) = request else { break; };
                                if let Request::ClearTimer(token) = request {
                                    clear_due = Some((Instant::now() + Duration::from_millis(2500), token));
                                    continue;
                                }
                                if let Err(e) = session.action(request).await { emit(&tx, &wake, Event::Error(e)); }
                                due = Some(Instant::now() + Duration::from_millis(250));
                            }
                            event = incoming.recv() => {
                                match event {
                                    Some(BoundedIncomingEvent::Command(command)) => {
                                        if command.headers.get("topic").is_some_and(|t| t == &session.topic) {
                                            let body: Value = serde_json::from_str(&command.body).unwrap_or_default();
                                            if body["action"] == "menu" { emit(&tx, &wake, Event::Toggle); }
                                            due = Some(Instant::now() + Duration::from_millis(250));
                                        }
                                    }
                                    Some(BoundedIncomingEvent::Overflow { .. }) => due = Some(Instant::now() + Duration::from_millis(250)),
                                    None => break,
                                }
                            }
                            changed = connection.changed() => {
                                if changed.is_err() { break; }
                                if session.client.is_connected() {
                                    due = Some(Instant::now() + Duration::from_millis(250));
                                } else { emit(&tx, &wake, Event::Error("Bus reconnecting".into())); }
                            }
                            _ = sleep_until(due.unwrap_or(far)), if due.is_some() => {
                                due = None;
                                let snapshot = session.refresh().await;
                                emit(&tx, &wake, Event::Snapshot(Box::new(snapshot)));
                            }
                            _ = sleep_until(clear_due.map(|v| v.0).unwrap_or(far)), if clear_due.is_some() => {
                                if let Some((_, token)) = clear_due.take() { emit(&tx, &wake, Event::ClearExpired(token)); }
                            }
                        }
                    }
                    session.client.close().await;
                    Ok::<(), String>(())
                }.await;
                if let Err(e) = result { emit(&tx, &wake, Event::Error(e)); }
            });
        });
        Self { requests, events }
    }
}

fn emit(tx: &flume::Sender<Event>, wake: &Arc<dyn Fn() + Send + Sync>, event: Event) {
    // Wake before and after sending so a full queue cannot deadlock a sleeping UI.
    wake();
    let _ = tx.send(event);
    wake();
}

struct Session {
    config: Config,
    client: SupervisedClient,
    local_instance: String,
    remote_instance: String,
    topic: String,
    query: String,
    snapshot: Snapshot,
}

impl Session {
    async fn connect(config: Config) -> Result<Self> {
        let client = SupervisedClient::connect_options(&config.name, &config.url)
            .bounded_incoming(256)
            .connect()
            .await
            .map_err(|e| e.to_string())?;
        Ok(Self {
            config,
            client,
            local_instance: String::new(),
            remote_instance: String::new(),
            topic: "desktop.clipboard.changed".into(),
            query: String::new(),
            snapshot: Snapshot::default(),
        })
    }

    async fn raw(&self, target: &str, verb: &str, body: Value) -> Result<(u8, Value)> {
        let (rc, text, _) = timeout(
            Duration::from_secs(8),
            self.client
                .call_with_headers_raw(target, verb, &BTreeMap::new(), &body.to_string()),
        )
        .await
        .map_err(|_| format!("{verb}: timeout"))?
        .map_err(|e| e.to_string())?;
        Ok((
            rc,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        ))
    }

    async fn capabilities(&mut self, remote: bool) -> Result<Value> {
        let target = if remote {
            &self.config.remote
        } else {
            &self.config.local
        };
        let (rc, caps) = self.raw(target, "desktop.capabilities", json!({})).await?;
        if rc != 0 || caps["api_version"] != 1 || caps["instance"].as_str().is_none() {
            return Err(format!("Capabilities unavailable: rc {rc}"));
        }
        let instance = caps["instance"].as_str().unwrap_or_default().to_owned();
        if remote {
            self.remote_instance = instance;
        } else {
            self.local_instance = instance;
            if let Some(topic) = caps
                .pointer("/clipboard/history/topic")
                .and_then(Value::as_str)
            {
                self.topic = topic.into();
            }
        }
        Ok(caps)
    }

    async fn rpc(&mut self, remote: bool, verb: &str, mut body: Value) -> Result<(u8, Value)> {
        for attempt in 0..2 {
            body["instance"] = json!(if remote {
                &self.remote_instance
            } else {
                &self.local_instance
            });
            let target = if remote {
                &self.config.remote
            } else {
                &self.config.local
            };
            let reply = self.raw(target, verb, body.clone()).await?;
            if reply.0 != 12 || attempt == 1 {
                return Ok(reply);
            }
            self.capabilities(remote).await?;
        }
        unreachable!()
    }

    async fn refresh(&mut self) -> Snapshot {
        let mut s = Snapshot::default();
        s.local = json!({"target":self.config.local,"instance":"","ok":false,"entries":0,"total":0,"revision":-1,"persistence":"unreachable","paused":false,"skipped":0,"subscribed":false,"topic":self.topic});
        s.remote = json!({"target":self.config.remote,"instance":"","ok":false,"entries":null});
        match self.capabilities(false).await {
            Ok(caps) => {
                let h = &caps["clipboard"]["history"];
                s.local["instance"] = json!(self.local_instance);
                for (key, fallback) in [
                    ("persistence", json!("?")),
                    ("paused", json!(false)),
                    ("skipped", json!(0)),
                ] {
                    s.local[key] = h.get(key).cloned().unwrap_or(fallback);
                }
                let subscribed = self
                    .client
                    .subscription_registry()
                    .snapshot()
                    .contains(&self.topic)
                    || self.client.subscribe_topic(&self.topic).await.is_ok();
                s.local["subscribed"] = json!(subscribed);
                s.local["topic"] = json!(self.topic);
                let mut body = json!({});
                if !self.query.is_empty() {
                    body["q"] = json!(self.query);
                }
                match self.rpc(false, "desktop.clipboard.history", body).await {
                    Ok((0, history)) if history["entries"].is_array() => {
                        s.entries = history["entries"].as_array().cloned().unwrap_or_default();
                        s.local["ok"] = json!(true);
                        s.local["entries"] = json!(s.entries.len());
                        s.local["total"] = history
                            .get("total")
                            .cloned()
                            .unwrap_or(json!(s.entries.len()));
                        s.local["revision"] = history.get("revision").cloned().unwrap_or(json!(-1));
                        s.local["instance"] = json!(self.local_instance);
                    }
                    other => s.error = format!("History unavailable: {other:?}"),
                }
            }
            Err(e) => s.error = e,
        }
        if !self.config.remote.is_empty() && self.capabilities(true).await.is_ok() {
            s.remote["instance"] = json!(self.remote_instance);
            if let Ok((0, history)) = self.rpc(true, "desktop.clipboard.history", json!({})).await {
                if let Some(entries) = history["entries"].as_array() {
                    s.remote_entries = entries.clone();
                    s.remote["ok"] = json!(true);
                    s.remote["entries"] = json!(entries.len());
                    s.remote["instance"] = json!(self.remote_instance);
                }
            }
        }
        self.snapshot = s.clone();
        s
    }

    async fn action(&mut self, request: Request) -> Result<()> {
        let (verb, body) = match request {
            Request::Search(q) => {
                self.query = q;
                return Ok(());
            }
            Request::Pick(id) => ("desktop.clipboard.pick", json!({"id":id})),
            Request::Clear => ("desktop.clipboard.clear", json!({})),
            Request::Pause(on) => ("desktop.clipboard.pause", json!({"on":on})),
            Request::RemotePick(id) => {
                let (rc, entry) = self
                    .rpc(true, "desktop.clipboard.entry", json!({"id":id}))
                    .await?;
                if rc != 0 || entry["id"] != id || !entry["text"].is_string() {
                    return Err(format!("Remote entry unavailable: rc {rc}"));
                }
                ("desktop.clipboard.write", json!({"text":entry["text"]}))
            }
            Request::ClearTimer(_) => return Ok(()),
        };
        let (rc, _) = self.rpc(false, verb, body).await?;
        if rc != 0 {
            return Err(format!("{verb}: rc {rc}"));
        }
        Ok(())
    }

    async fn smoke_verbs(&mut self, incoming: &mut BoundedIncomingReceiver) -> Result<Value> {
        let mut verbs = json!({});
        for (key, verb, body) in [
            ("pause_on", "desktop.clipboard.pause", json!({"on":true})),
            ("pause_off", "desktop.clipboard.pause", json!({"on":false})),
            ("search", "desktop.clipboard.history", json!({"q":"e"})),
        ] {
            let (rc, mut reply) = self.rpc(false, verb, body).await?;
            if !reply.is_object() {
                reply = json!({});
            }
            reply["rc"] = json!(rc);
            // Smoke reports counts, never clipboard contents.
            if let Some(object) = reply.as_object_mut() {
                object.remove("entries");
            }
            verbs[key] = reply;
        }
        if let Some(entry) = self.snapshot.entries.first() {
            let id = entry["id"].clone();
            let (rc, mut reply) = self
                .rpc(false, "desktop.clipboard.pick", json!({"id":id}))
                .await?;
            if !reply.is_object() {
                reply = json!({});
            }
            reply["rc"] = json!(rc);
            verbs["pick"] = reply;
        } else {
            verbs["pick"] = json!("skipped: empty history");
        }
        let (rc, mut reply) = self.rpc(false, "desktop.clipboard.menu", json!({})).await?;
        if !reply.is_object() {
            reply = json!({});
        }
        reply["rc"] = json!(rc);
        verbs["menu"] = reply;
        let event = timeout(Duration::from_secs(3), async {
            while let Some(event) = incoming.recv().await {
                if let BoundedIncomingEvent::Command(command) = event {
                    if command.headers.get("topic") != Some(&self.topic) { continue; }
                    let body: Value = serde_json::from_str(&command.body).unwrap_or_default();
                    if body["action"] == "menu" {
                        return json!({"topic":self.topic,"action":body["action"],"revision":body["revision"]});
                    }
                }
            }
            json!("NOT RECEIVED within 3s")
        }).await.unwrap_or(json!("NOT RECEIVED within 3s"));
        verbs["event"] = event;
        Ok(verbs)
    }
}

pub fn smoke(mode: &str) -> i32 {
    let output = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("clippanel_smoke.out");
    let result = runtime().block_on(async {
        timeout(Duration::from_secs(15), async {
            let mut session = Session::connect(Config::load()?).await?;
            let mut incoming = session
                .client
                .incoming_bounded()
                .ok_or("No incoming lane")?;
            let snapshot = session.refresh().await;
            let verbs = if mode == "verbs" && !session.local_instance.is_empty() {
                session.smoke_verbs(&mut incoming).await?
            } else {
                json!({})
            };
            let report = json!({"bus":"ready","name":session.config.name,"url":session.config.url,
            "local":snapshot.local,"remote":snapshot.remote,"verbs":verbs});
            session.client.close().await;
            Ok::<Value, String>(report)
        })
        .await
    });
    let (text, code) = match result {
        Ok(Ok(report)) => (format!("{report:#}\n"), 0),
        Ok(Err(e)) => (format!("ERROR {e}\n"), 1),
        Err(_) => ("TIMEOUT clipboard smoke exceeded 15s\n".into(), 1),
    };
    if let Err(e) = std::fs::write(output, text) {
        eprintln!("smoke output: {e}");
        return 1;
    }
    code
}
