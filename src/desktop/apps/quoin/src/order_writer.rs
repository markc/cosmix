//! `shell.panel.order` off the render thread (Stage R, GLM m10).
//!
//! The verb is mesh-open and its write fsyncs, so it must not run inside a
//! Bevy `Update` system: a caller looping it would stall frames. `service_bus`
//! hands the request to one writer thread, which performs the whole
//! read-validate-replace in arrival order and sends the reply back; a
//! Presentation system answers it. The reply therefore still means the file
//! is durably replaced (data fsync, rename, directory fsync). The thread
//! wakes the layer host when a reply is ready, so an idle host answers
//! without waiting for another event.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender, channel};

use bevy::prelude::*;
use cosmix_shell::runtime::ShellRuntimeSet;
use ctk::bus::{BusBridge, InboundRequest};
use serde_json::Value;

type Wake = Arc<dyn Fn() + Send + Sync>;

#[derive(Resource)]
pub(crate) struct OrderWriter {
    jobs: Sender<(InboundRequest, PathBuf)>,
    done: Mutex<Receiver<(InboundRequest, u8, Value)>>,
    /// Replies that met a full outbound channel, retried as answered.
    unsent: Mutex<Vec<(InboundRequest, u8, Value)>>,
}

impl OrderWriter {
    fn spawn(wake: Option<Wake>) -> Self {
        let (jobs, inbox) = channel::<(InboundRequest, PathBuf)>();
        let (outbox, done) = channel();
        std::thread::Builder::new()
            .name("quoin-conf-order".into())
            .spawn(move || {
                for (request, path) in inbox {
                    let (rc, body) = crate::bus_service::panel_order(&request.body, &path);
                    if outbox.send((request, rc, body)).is_err() {
                        break;
                    }
                    if let Some(wake) = &wake {
                        wake();
                    }
                }
            })
            .expect("spawn the conf.mix order writer");
        Self {
            jobs,
            done: Mutex::new(done),
            unsent: Mutex::new(Vec::new()),
        }
    }

    /// Queue one validated-later write; false when the writer is gone.
    pub(crate) fn submit(&self, request: InboundRequest, path: PathBuf) -> bool {
        self.jobs.send((request, path)).is_ok()
    }

    fn ready(&self) -> Vec<(InboundRequest, u8, Value)> {
        let mut replies = std::mem::take(&mut *self.unsent.lock().expect("unsent lock"));
        replies.extend(self.done.lock().expect("order inbox lock").try_iter());
        replies
    }
}

/// Install after the host inserted its wake (the layer host's
/// `LayerHostWake`); an embedded host without one answers on its next update.
pub(crate) fn install(app: &mut App) {
    let wake = app
        .world()
        .get_resource::<cosmix_shell_host::LayerHostWake>()
        .map(cosmix_shell_host::LayerHostWake::callback);
    app.insert_resource(OrderWriter::spawn(wake))
        .add_systems(Update, reply_orders.in_set(ShellRuntimeSet::Presentation));
}

fn reply_orders(bridge: Res<BusBridge>, writer: Res<OrderWriter>) {
    let mut retry = Vec::new();
    for (request, rc, body) in writer.ready() {
        if let Err(error) = bridge.try_respond(&request, rc, body.to_string()) {
            if bridge.worker_is_gone() {
                warn!("shell Bus worker has stopped; dropping panel.order reply ({error})");
            } else {
                retry.push((request, rc, body));
            }
        }
    }
    writer.unsent.lock().expect("unsent lock").extend(retry);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    #[test]
    fn writes_happen_off_thread_in_order_and_answer_after_the_file_is_replaced() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("conf.mix");
        std::fs::write(&path, r#"{panels: {right: ["scene-notes"]}}"#).unwrap();
        let (tx, woken) = channel();
        let wake: Wake = Arc::new(move || {
            let _ = tx.send(());
        });
        let writer = OrderWriter::spawn(Some(wake));
        let request = |order: Value| InboundRequest {
            connection_generation: 1,
            from: "peer".into(),
            command: "shell.panel.order".into(),
            headers: Default::default(),
            body: json!({"edges": order}).to_string(),
            reply_id: Some("1".into()),
        };
        assert!(writer.submit(request(json!({"right": ["a", "b"]})), path.clone()));
        assert!(writer.submit(request(json!({"right": ["b", "a"]})), path.clone()));
        let mut replies = Vec::new();
        while replies.len() < 2 {
            woken.recv_timeout(Duration::from_secs(10)).expect("the writer wakes the host");
            replies.extend(writer.ready());
        }
        assert_eq!(
            replies.iter().map(|(_, rc, body)| (*rc, body["edges"]["right"].clone())).collect::<Vec<_>>(),
            [(0, json!(["a", "b"])), (0, json!(["b", "a"]))],
            "answered in arrival order"
        );
        let written =
            crate::config::ShellConfig::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written.panels[cosmix_shell::core::Edge::Right.index()], ["b", "a"]);
    }
}
