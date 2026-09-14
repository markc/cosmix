//! Native ABP discovery and calls; no network work runs on the Bevy thread.
use std::{collections::BTreeMap, sync::Arc, time::Duration};

use bevy::prelude::Resource;
use cosmix_client::NodedClient;
use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub struct Verb {
    pub name: String,
    pub args: String,
    pub description: String,
    pub read_only: Option<bool>,
}

pub fn parse_verbs(value: &Value) -> Result<Vec<Verb>, String> {
    let entries = value
        .as_array()
        .or_else(|| value.get("verbs")?.as_array())
        .ok_or("Expected a HELP array or app.describe verbs array")?;
    let mut verbs = Vec::new();
    for entry in entries {
        let name = entry
            .as_str()
            .or_else(|| entry.get("name")?.as_str())
            .filter(|name| !name.is_empty())
            .ok_or("Verb has no name")?;
        verbs.push(Verb {
            name: name.into(),
            args: entry
                .get("args")
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| v.to_string())
                })
                .unwrap_or_default(),
            description: entry
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("Description unavailable on this service")
                .into(),
            read_only: entry.get("read_only").and_then(Value::as_bool),
        });
    }
    verbs.sort_by(|a, b| a.name.cmp(&b.name));
    verbs.dedup_by(|a, b| a.name == b.name);
    Ok(verbs)
}

pub fn peer_names(value: &Value) -> Vec<String> {
    let local = value.get("node").and_then(Value::as_str);
    let mut peers: Vec<String> = value
        .pointer("/authority/routing_view")
        .or_else(|| value.get("peers"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|v| v.get("name")?.as_str())
        .filter(|name| Some(*name) != local)
        .map(str::to_owned)
        .collect();
    peers.sort();
    peers.dedup();
    peers
}

pub enum Request {
    Discover,
    Call {
        service: String,
        verb: String,
        body: String,
    },
}

pub enum Event {
    Services(Vec<String>),
    Peers(Result<Vec<String>, String>),
    Verbs(String, Result<Vec<Verb>, String>),
    Discovered(Result<(), String>),
    Reply(String),
}

#[derive(Resource)]
pub struct Bus {
    pub requests: flume::Sender<Request>,
    pub events: flume::Receiver<Event>,
}

impl Bus {
    pub fn start(url: Option<String>) -> Self {
        let (requests, receiver) = flume::bounded::<Request>(8);
        let (sender, events) = flume::bounded(256);
        std::thread::Builder::new().name("busviewer-bus".into()).spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2)
                .enable_all().build().expect("BusViewer Tokio runtime");
            runtime.block_on(async move {
                while let Ok(request) = receiver.recv_async().await {
                    let sender = sender.clone();
                    let url = url.clone();
                    tokio::spawn(async move {
                        let result = async {
                            let url = url.ok_or("No node configuration; use --noded-url URL".to_owned())?;
                            let client = tokio::time::timeout(Duration::from_secs(10), NodedClient::connect_anonymous(&url))
                                .await.map_err(|_| "Bus connection timed out".to_owned())?
                                .map_err(|e| e.to_string())?;
                            match &request {
                                Request::Discover => discover(Arc::new(client), &sender).await,
                                Request::Call { service, verb, body } => {
                                    let reply = raw_call(&client, service, verb, body).await?;
                                    let text = format!("{service}  {verb}\nrc = {}\n\n{}", reply.0, pretty_body(&reply.1));
                                    let _ = sender.send_async(Event::Reply(text)).await;
                                    Ok(())
                                }
                            }
                        }.await;
                        match request {
                            Request::Discover => { let _ = sender.send_async(Event::Discovered(result)).await; }
                            Request::Call { service, verb, .. } => if let Err(error) = result {
                                let _ = sender.send_async(Event::Reply(format!("{service}  {verb}\nTransport error: {error}\nThe call was not retried; its outcome may be unknown."))).await;
                            }
                        }
                    });
                }
            });
        }).expect("BusViewer worker thread");
        Self { requests, events }
    }
}

pub fn pretty_body(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| body.to_owned())
}

async fn raw_call(
    client: &NodedClient,
    service: &str,
    verb: &str,
    body: &str,
) -> Result<(u8, String), String> {
    tokio::time::timeout(
        Duration::from_secs(20),
        client.call_with_headers_raw(service, verb, &BTreeMap::new(), body),
    )
    .await
    .map_err(|_| "Call timed out after 20 seconds".to_owned())?
    .map(|(rc, body, _)| (rc, body))
    .map_err(|e| e.to_string())
}

async fn json_call(client: &NodedClient, service: &str, verb: &str) -> Result<Value, String> {
    let (rc, body) = raw_call(client, service, verb, "").await?;
    if rc >= 10 {
        return Err(format!("rc = {rc}: {body}"));
    }
    serde_json::from_str(&body).map_err(|e| e.to_string())
}

async fn describe(client: &NodedClient, service: &str) -> Result<Vec<Verb>, String> {
    let help = match json_call(client, service, "HELP").await {
        Ok(value) => parse_verbs(&value),
        Err(error) => Err(error),
    };
    match help {
        Ok(verbs) => Ok(verbs),
        Err(help_error) => match json_call(client, service, "app.describe").await {
            Ok(value) => parse_verbs(&value),
            Err(error) => Err(format!("HELP: {help_error}\napp.describe: {error}")),
        },
    }
}

async fn discover(client: Arc<NodedClient>, events: &flume::Sender<Event>) -> Result<(), String> {
    let list = json_call(&client, "noded", "noded.list").await?;
    let mut services: Vec<String> = list
        .as_array()
        .ok_or("noded.list is not an array")?
        .iter()
        .filter_map(|v| v.get("name")?.as_str())
        .map(str::to_owned)
        .collect();
    // The broker is not necessarily registered in its own citizen list.
    services.push("noded".into());
    services.sort();
    services.dedup();
    let _ = events.send_async(Event::Services(services.clone())).await;
    let peer_client = client.clone();
    let peer_events = events.clone();
    let peers = tokio::spawn(async move {
        let result = json_call(&peer_client, "noded", "noded.peers")
            .await
            .map(|v| peer_names(&v));
        let _ = peer_events.send_async(Event::Peers(result)).await;
    });
    // Bound concurrent introspection, but stream results as each service responds.
    let mut pending = tokio::task::JoinSet::new();
    let mut names = services.into_iter();
    loop {
        while pending.len() < 8 {
            let Some(service) = names.next() else {
                break;
            };
            let client = client.clone();
            let events = events.clone();
            pending.spawn(async move {
                let verbs = describe(&client, &service).await;
                let _ = events.send_async(Event::Verbs(service, verbs)).await;
            });
        }
        match pending.join_next().await {
            None => break,
            Some(Err(error)) => return Err(error.to_string()),
            Some(Ok(())) => {}
        }
    }
    peers.await.map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn accepts_universal_and_legacy_descriptions_without_inventing_safety() {
        let verbs = parse_verbs(
            &json!([{"name":"ping", "args":[], "description":"Probe", "read_only":true}]),
        )
        .unwrap();
        assert_eq!(verbs[0].read_only, Some(true));
        let legacy = parse_verbs(&json!({"verbs":["quit", "ping"]})).unwrap();
        assert_eq!(legacy[0].name, "ping");
        assert_eq!(legacy[1].read_only, None);
        assert!(parse_verbs(&json!({"title":"Old app"})).is_err());
    }

    #[test]
    fn peers_use_authority_and_exclude_self_with_legacy_fallback() {
        assert_eq!(
            peer_names(
                &json!({"node":"alpha", "authority":{"routing_view":[{"name":"alpha"},{"name":"beta"}]}, "peers":[{"name":"stale"}]})
            ),
            vec!["beta"]
        );
        assert_eq!(
            peer_names(&json!({"peers":[{"name":"beta"}]})),
            vec!["beta"]
        );
    }

    #[test]
    fn reply_preserves_non_json_errors() {
        assert_eq!(pretty_body("permission denied"), "permission denied");
        assert!(pretty_body("{\"ok\":true}").contains('\n'));
    }

    #[test]
    #[ignore = "requires the configured local noded; performs read-only discovery"]
    fn live_native_discovery_and_reply() {
        let url = ctk::prelude::configured_noded_url().expect("configured local broker");
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let client = Arc::new(NodedClient::connect_anonymous(&url).await.unwrap());
            let (sender, receiver) = flume::unbounded();
            discover(client.clone(), &sender).await.unwrap();
            let events: Vec<_> = receiver.try_iter().collect();
            assert!(events.iter().any(|e| matches!(e, Event::Services(names) if names.iter().any(|n| n == "noded"))));
            // A running broker may predate HELP, and the broker itself does
            // not share the citizen-client HELP handler. Partial discovery
            // must still expose the services that can describe themselves.
            let described = events.iter().filter(|e| matches!(e, Event::Verbs(_, Ok(verbs)) if !verbs.is_empty())).count();
            let unavailable = events.iter().filter(|e| matches!(e, Event::Verbs(_, Err(_)))).count();
            println!("Live ABP discovery: {described} services with verbs, {unavailable} unavailable descriptions");
            assert!(described > 0, "no live service supplied a usable verb description");
            assert!(events.iter().any(|e| matches!(e, Event::Peers(Ok(_)))));
            let (rc, body) = raw_call(&client, "noded", "noded.list", "{}").await.unwrap();
            assert!(rc < 10);
            assert!(serde_json::from_str::<Value>(&body).unwrap().is_array());
        });
    }
}
