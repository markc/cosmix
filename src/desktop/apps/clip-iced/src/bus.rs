//! One native Bus path shared by the GUI and the headless acceptance gate.
use cosmix_client::{BoundedIncomingEvent, BoundedIncomingReceiver, SupervisedClient};
use iced::futures::{SinkExt, Stream};
use rand::RngCore;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::BTreeMap, time::Duration};
use tokio::sync::mpsc;

#[derive(Clone, Debug, Deserialize)]
pub struct Entry {
    pub id: Value,
    pub bytes: u64,
    pub at: f64,
    pub preview: String,
}

#[derive(Clone, Debug)]
pub struct Endpoint {
    pub target: String,
    pub instance: String,
    pub entries: Vec<Entry>,
    pub ok: bool,
    pub total: u64,
    pub revision: i64,
    pub persistence: String,
    pub paused: bool,
    pub skipped: u64,
    pub topic: String,
    pub subscribed: bool,
}

impl Endpoint {
    fn new(target: String) -> Self {
        Self {
            target,
            instance: String::new(),
            entries: vec![],
            ok: false,
            total: 0,
            revision: -1,
            persistence: "?".into(),
            paused: false,
            skipped: 0,
            topic: "desktop.clipboard.changed".into(),
            subscribed: false,
        }
    }

    async fn capabilities(&mut self, client: &SupervisedClient) -> bool {
        let reply = raw(client, &self.target, "desktop.capabilities", json!({})).await;
        if reply.rc != 0 || reply.body["api_version"] != 1 || !reply.body["instance"].is_string() {
            self.instance.clear();
            self.persistence = "unreachable".into();
            return false;
        }
        self.instance = string(&reply.body["instance"]);
        let h = &reply.body["clipboard"]["history"];
        self.persistence = h["persistence"].as_str().unwrap_or("?").into();
        self.paused = h["paused"].as_bool().unwrap_or(false);
        self.skipped = h["skipped"].as_u64().unwrap_or(0);
        self.topic = h["topic"]
            .as_str()
            .unwrap_or("desktop.clipboard.changed")
            .into();
        true
    }

    async fn rpc(&mut self, client: &SupervisedClient, verb: &str, mut body: Value) -> Reply {
        body["instance"] = json!(self.instance);
        let reply = raw(client, &self.target, verb, body.clone()).await;
        if reply.rc == 12 && self.capabilities(client).await {
            body["instance"] = json!(self.instance);
            return raw(client, &self.target, verb, body).await;
        }
        reply
    }

    async fn refresh(&mut self, client: &SupervisedClient, query: &str, subscribe: bool) {
        self.ok = false;
        self.entries.clear();
        self.total = 0;
        self.revision = -1;
        if self.target.is_empty() || !self.capabilities(client).await {
            return;
        }
        if subscribe {
            self.subscribed = client
                .subscription_registry()
                .snapshot()
                .contains(&self.topic)
                || client.subscribe_topic(&self.topic).await.is_ok();
        }
        let mut body = json!({});
        if !query.is_empty() {
            body["q"] = json!(query);
        }
        let reply = self.rpc(client, "desktop.clipboard.history", body).await;
        if reply.rc == 0 {
            if let Ok(entries) = serde_json::from_value::<Vec<Entry>>(reply.body["entries"].clone())
            {
                self.entries = entries;
                self.total = reply.body["total"]
                    .as_u64()
                    .unwrap_or(self.entries.len() as u64);
                self.revision = reply.body["revision"].as_i64().unwrap_or(-1);
                self.ok = true;
            }
        }
    }

    fn report(&self, remote: bool) -> Value {
        json!({"target":self.target, "instance":self.instance, "ok":self.ok,
            "entries":if remote && !self.ok { Value::Null } else { json!(self.entries.len()) },
            "total":self.total, "revision":self.revision, "persistence":self.persistence,
            "paused":self.paused, "subscribed":self.subscribed, "topic":self.topic})
    }
}

pub fn string(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub local: Endpoint,
    pub remote: Endpoint,
}

impl Default for Snapshot {
    fn default() -> Self {
        let data = std::fs::read_to_string("/etc/cosmix/clipboard.conf.mix").unwrap_or_default();
        let target = |key: &str| {
            regex::Regex::new(&format!(r#"\b{key}"?\s*:\s*"([^"]+)""#))
                .expect("literal target regex")
                .captures(&data)
                .map(|c| c[1].to_owned())
        };
        Self {
            local: Endpoint::new(target("local").unwrap_or("desktop-vt1".into())),
            remote: Endpoint::new(target("remote").unwrap_or_default()),
        }
    }
}

struct Session {
    client: SupervisedClient,
    incoming: BoundedIncomingReceiver,
    data: Snapshot,
    name: String,
    url: String,
}

impl Session {
    async fn connect() -> Result<Self, String> {
        let config = std::fs::read_to_string("/etc/cosmix/node.conf.mix").unwrap_or_default();
        let capture = |pattern| {
            regex::Regex::new(pattern)
                .expect("literal config regex")
                .captures(&config)
                .map(|c| c[1].to_owned())
        };
        let host = capture(r#""wg_ip"\s*:\s*"([^"]+)""#).unwrap_or("127.0.0.1".into());
        let port = capture(r#""noded"\s*:\s*\{[^}]*"port"\s*:\s*(\d+)"#).unwrap_or("4200".into());
        let host = if host.contains(':') {
            format!("[{host}]")
        } else {
            host
        };
        let url = format!("ws://{host}:{port}/ws");
        let mut nonce = [0u8; 10];
        rand::thread_rng().fill_bytes(&mut nonce);
        let name = format!(
            "clippanel-{}",
            nonce.iter().map(|b| format!("{b:02x}")).collect::<String>()
        );
        let client = SupervisedClient::connect_options(&name, &url)
            .bounded_incoming(256)
            .connect()
            .await
            .map_err(|e| e.to_string())?;
        let incoming = client.incoming_bounded().ok_or("missing incoming lane")?;
        Ok(Self {
            client,
            incoming,
            data: Snapshot::default(),
            name,
            url,
        })
    }

    async fn refresh(&mut self, query: &str) {
        tokio::join!(
            self.data.local.refresh(&self.client, query, true),
            self.data.remote.refresh(&self.client, "", false)
        );
    }

    async fn action(&mut self, action: Action) -> Reply {
        let (verb, body) = match action {
            Action::Pick(id) => ("desktop.clipboard.pick", json!({"id":id})),
            Action::Clear => ("desktop.clipboard.clear", json!({})),
            Action::Pause(on) => ("desktop.clipboard.pause", json!({"on":on})),
            Action::RemotePick(id) => {
                let reply = self
                    .data
                    .remote
                    .rpc(&self.client, "desktop.clipboard.entry", json!({"id":id}))
                    .await;
                if reply.rc != 0 || reply.body["id"] != id || !reply.body["text"].is_string() {
                    return reply;
                }
                (
                    "desktop.clipboard.write",
                    json!({"text":reply.body["text"]}),
                )
            }
        };
        self.data.local.rpc(&self.client, verb, body).await
    }
}

struct Reply {
    rc: i32,
    body: Value,
}
impl Reply {
    fn report(self) -> Value {
        let mut body = if self.body.is_object() {
            self.body
        } else {
            json!({"body":self.body})
        };
        body["rc"] = json!(self.rc);
        body
    }
}

async fn raw(client: &SupervisedClient, target: &str, verb: &str, body: Value) -> Reply {
    match tokio::time::timeout(
        Duration::from_secs(8),
        client.call_with_headers_raw(target, verb, &BTreeMap::new(), &body.to_string()),
    )
    .await
    {
        Ok(Ok((rc, body, _))) => Reply {
            rc: rc.into(),
            body: serde_json::from_str(&body).unwrap_or(json!(body)),
        },
        Ok(Err(e)) => Reply {
            rc: -2,
            body: json!(e.to_string()),
        },
        Err(_) => Reply {
            rc: -1,
            body: json!("timeout"),
        },
    }
}

#[derive(Clone, Debug)]
pub enum Action {
    Pick(Value),
    RemotePick(Value),
    Pause(bool),
    Clear,
}
#[derive(Clone, Debug)]
pub enum Request {
    Query(String),
    Action(Action),
}
#[derive(Clone, Debug)]
pub enum Event {
    Ready(mpsc::Sender<Request>),
    Snapshot(Box<Snapshot>),
    Toggle,
    Error(String),
}

pub fn events() -> impl Stream<Item = Event> {
    iced::stream::channel(32, async |mut output| {
        let (sender, mut requests) = mpsc::channel(32);
        let _ = output.send(Event::Ready(sender)).await;
        // A failed initial connection is retried without waking the UI every frame.
        let mut session = loop {
            match Session::connect().await {
                Ok(s) => break s,
                Err(e) => {
                    let _ = output.send(Event::Error(e)).await;
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        };
        let mut state = session.client.subscribe_state();
        let mut query = String::new();
        let mut refresh = Some(tokio::time::Instant::now() + Duration::from_millis(250));
        loop {
            let wait = async {
                match refresh {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                _ = wait => {
                    session.refresh(&query).await;
                    if output.send(Event::Snapshot(Box::new(session.data.clone()))).await.is_err() { break; }
                    refresh = None;
                }
                request = requests.recv() => {
                    match request {
                        Some(Request::Query(q)) => query = q,
                        Some(Request::Action(action)) => {
                            let reply = session.action(action).await;
                            if reply.rc != 0 { let _ = output.send(Event::Error(format!("Action failed (rc {})", reply.rc))).await; }
                        }
                        None => break,
                    }
                    refresh = Some(tokio::time::Instant::now() + Duration::from_millis(250));
                }
                event = session.incoming.recv() => {
                    match event {
                        Some(BoundedIncomingEvent::Command(c)) if c.header("topic") == Some(session.data.local.topic.as_str()) => {
                            if c.args["action"] == "menu" { let _ = output.send(Event::Toggle).await; }
                        }
                        Some(BoundedIncomingEvent::Overflow { .. }) => { let _ = output.send(Event::Error("Bus event overflow; refreshing".into())).await; }
                        Some(_) => continue,
                        None => break,
                    }
                    refresh = Some(tokio::time::Instant::now() + Duration::from_millis(250));
                }
                changed = state.changed() => {
                    if changed.is_err() { break; }
                    if session.client.is_connected() {
                        refresh = Some(tokio::time::Instant::now() + Duration::from_millis(250));
                    } else { let _ = output.send(Event::Error("Bus reconnecting".into())).await; }
                }
            }
        }
    })
}

pub async fn smoke(mode: &str) -> i32 {
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        let mut session = Session::connect().await?;
        tokio::time::sleep(Duration::from_millis(250)).await;
        session.refresh("").await;
        let local = session.data.local.report(false);
        let remote = session.data.remote.report(true);
        let mut verbs = json!({});
        if mode == "verbs" && !session.data.local.instance.is_empty() {
            verbs["pause_on"] = session.action(Action::Pause(true)).await.report();
            verbs["pause_off"] = session.action(Action::Pause(false)).await.report();
            verbs["search"] = session.data.local.rpc(&session.client, "desktop.clipboard.history", json!({"q":"e"})).await.report();
            if let Some(entry) = session.data.local.entries.first() {
                verbs["pick"] = session.action(Action::Pick(entry.id.clone())).await.report();
            } else { verbs["pick"] = json!("skipped: empty history"); }
            // Drain earlier pause/pick and retained menu deliveries, then await
            // a fresh menu event (not merely the first changed event).
            while tokio::time::timeout(Duration::from_millis(1), session.incoming.recv()).await.is_ok() {}
            verbs["menu"] = session.data.local.rpc(&session.client, "desktop.clipboard.menu", json!({})).await.report();
            let event = tokio::time::timeout(Duration::from_secs(3), async {
                while let Some(event) = session.incoming.recv().await {
                    if let BoundedIncomingEvent::Command(c) = event {
                        if c.header("topic") == Some(session.data.local.topic.as_str()) && c.args["action"] == "menu" {
                            return json!({"topic":c.header("topic"), "action":c.args["action"], "revision":c.args["revision"]});
                        }
                    }
                }
                json!("NOT RECEIVED within 3s")
            }).await.unwrap_or(json!("NOT RECEIVED within 3s"));
            verbs["event"] = event;
        }
        Ok::<_, String>(json!({"bus":"ready", "name":session.name, "url":session.url,
            "local":local, "remote":remote, "verbs":verbs}))
    }).await;
    let (output, code) = match result {
        Ok(Ok(value)) => (format!("{value:#}\n"), 0),
        Ok(Err(e)) => (format!("ERROR {e}\n"), 1),
        Err(_) => ("TIMEOUT clipboard smoke exceeded 15s\n".into(), 1),
    };
    let path = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => std::path::PathBuf::from(dir).join("clippanel_smoke.out"),
        None => {
            eprintln!("XDG_RUNTIME_DIR is required for smoke output");
            return 1;
        }
    };
    if let Err(e) = std::fs::write(path, output) {
        eprintln!("smoke output: {e}");
        return 1;
    }
    code
}
