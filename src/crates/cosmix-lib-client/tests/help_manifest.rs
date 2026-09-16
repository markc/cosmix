//! Exercise the real reader and writer over a WebSocket, without a broker.
#![cfg(feature = "native")]

use cosmix_bus::{
    VerbDescriptor,
    bus::{self, BusMessage},
};
use cosmix_client::NodedClient;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::time::{Duration, timeout};
use tokio_tungstenite::tungstenite::Message;

async fn exercise(manifest: Option<Vec<VerbDescriptor>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        tokio_tungstenite::accept_async(socket).await.unwrap()
    });
    let client = NodedClient::connect_anonymous(&url).await.unwrap();
    let client = match &manifest {
        Some(verbs) => client.with_verbs(verbs.clone()),
        None => client,
    };
    let mut server = server.await.unwrap();
    let mut incoming = client.incoming_async().await.unwrap();

    for id in [Some("help-1"), None] {
        let mut request = BusMessage::new()
            .with_header("command", "HELP")
            .with_header("type", "request")
            .with_header("from", "caller")
            .with_header("to", "anonymous");
        if let Some(id) = id {
            request = request.with_header("id", id);
        }
        server
            .send(Message::Text(request.to_wire().into()))
            .await
            .unwrap();
        if manifest.is_none() {
            let command = incoming.recv().await.unwrap();
            assert_eq!(command.command, "HELP");
            client
                .respond(&command, 0, "application HELP")
                .await
                .unwrap();
        }
        let wire = server.next().await.unwrap().unwrap().into_text().unwrap();
        let reply = bus::parse(&wire).unwrap();
        assert_eq!(reply.get("type"), Some("response"));
        assert_eq!(reply.get("command"), Some("HELP"));
        assert_eq!(reply.get("from"), Some("anonymous"));
        assert_eq!(reply.get("to"), Some("caller"));
        assert_eq!(reply.get("id"), id);
        assert_eq!(reply.get("rc"), Some("0"));
        if let Some(verbs) = &manifest {
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&reply.body).unwrap(),
                serde_json::to_value(verbs).unwrap()
            );
        } else {
            assert_eq!(reply.body, "application HELP");
        }

        // A marker proves the reader advanced and HELP was consumed. A
        // service with its own HELP handler cannot accidentally answer twice.
        let marker = BusMessage::new()
            .with_header("command", "example.status")
            .with_header("type", "request");
        server
            .send(Message::Text(marker.to_wire().into()))
            .await
            .unwrap();
        assert_eq!(incoming.recv().await.unwrap().command, "example.status");
    }
    assert!(
        timeout(Duration::from_millis(20), server.next())
            .await
            .is_err(),
        "no duplicate replies"
    );
    client.close().await;
}

#[tokio::test]
async fn topic_delivery_does_not_require_command_or_event_type() {
    timeout(Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            tokio_tungstenite::accept_async(socket).await.unwrap()
        });
        let client = NodedClient::connect_anonymous(&url).await.unwrap();
        let mut server = server.await.unwrap();
        let mut incoming = client.incoming_async().await.unwrap();
        for kind in 0..3 {
            let mut message = BusMessage::new().with_header("topic", "example.changed");
            if kind == 0 {
                message = message.with_header("type", "event");
            } else if kind == 1 {
                message = message.with_header("command", "example.changed");
            }
            message.body = r#"{"action":"menu"}"#.into();
            server
                .send(Message::Text(message.to_wire().into()))
                .await
                .unwrap();
            let delivered = incoming.recv().await.unwrap();
            assert_eq!(
                delivered.headers.get("topic").map(String::as_str),
                Some("example.changed")
            );
            assert_eq!(delivered.body, message.body);
            assert_eq!(
                delivered.command,
                if kind == 1 { "example.changed" } else { "" }
            );
        }
        client.close().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn reader_consumes_help_only_when_manifest_is_present() {
    timeout(Duration::from_secs(5), async {
        exercise(None).await;
        exercise(Some(vec![])).await;
        exercise(Some(vec![
            VerbDescriptor::new("HELP", &[], "List commands", true),
            VerbDescriptor::new("example.set", &["key", "value"], "Set a value", false),
        ]))
        .await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn supervised_manifest_is_ready_before_registration_and_survives_reconnect() {
    timeout(Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let manifest = vec![VerbDescriptor::new("HELP", &[], "List commands", true)];
        let expected = serde_json::to_string(&manifest).unwrap();
        let (advance, mut next) = tokio::sync::mpsc::channel(1);
        let server = tokio::spawn(async move {
            for generation in 1..=2 {
                let (socket, _) = listener.accept().await.unwrap();
                let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
                let wire = socket.next().await.unwrap().unwrap().into_text().unwrap();
                let register = bus::parse(&wire).unwrap();
                assert_eq!(register.get("command"), Some("noded.register"));
                let help = BusMessage::new()
                    .with_header("type", "request")
                    .with_header("command", "HELP")
                    .with_header("from", "caller")
                    .with_header("id", "early-help");
                socket
                    .send(Message::Text(help.to_wire().into()))
                    .await
                    .unwrap();
                let wire = socket.next().await.unwrap().unwrap().into_text().unwrap();
                let reply = bus::parse(&wire).unwrap();
                assert_eq!(reply.get("from"), Some("example"));
                assert_eq!(reply.get("id"), Some("early-help"));
                assert_eq!(reply.get("rc"), Some("0"));
                assert_eq!(reply.body, expected);
                let ack = BusMessage::new()
                    .with_header("type", "response")
                    .with_header("command", "noded.register")
                    .with_header("id", register.get("id").unwrap())
                    .with_header("rc", "0");
                socket
                    .send(Message::Text(ack.to_wire().into()))
                    .await
                    .unwrap();
                let marker = BusMessage::new()
                    .with_header("command", "example.marker")
                    .with_header("type", "request")
                    .with_header("id", &generation.to_string());
                socket
                    .send(Message::Text(marker.to_wire().into()))
                    .await
                    .unwrap();
                next.recv().await.unwrap();
                socket.close(None).await.unwrap();
            }
        });
        let client = cosmix_client::SupervisedClient::connect_options("example", &url)
            .bounded_incoming(1)
            .with_verbs(manifest)
            .connect()
            .await
            .unwrap();
        let mut incoming = client.incoming_bounded().unwrap();
        for generation in 1..=2 {
            let Some(cosmix_client::BoundedIncomingEvent::Command(command)) = incoming.recv().await
            else {
                panic!("expected marker, no HELP or overflow");
            };
            assert_eq!(command.command, "example.marker");
            assert_eq!(command.id, Some(generation.to_string()));
            advance.send(()).await.unwrap();
        }
        server.await.unwrap();
        client.close().await;
    })
    .await
    .unwrap();
}
