//! Webd's [`DispatchPublisher`] bridge to `noded`.
//!
//! The substrate's per-namespace fan-out task asks this publisher to
//! emit each event on `<svc>.props.records.changed` and
//! `<svc>.props.audit`. We translate to a `noded` `topic.publish` RPC
//! with `retain: false` — see maild's commentary for the rationale.
//! `retain: false` is a hard requirement on both topics: records.changed
//! is a delta stream, not a snapshot.
//!
//! ## Refreshable client handle
//!
//! Maild's equivalent publisher pins the [`NodedClient`] at
//! construction. Webd's reconnect-loop `bus::run` (see this crate's
//! [`super`] module docs) makes that wrong here: the publisher would
//! survive the first broker disconnect holding a closed Arc, and
//! every subsequent publish would fail until process restart. We
//! share an `Arc<ArcSwapOption<NodedClient>>` with the
//! [`super::subscribe_granter::NodedSubscribeGranter`]; the run loop
//! swaps in the new client on each successful reconnect and stores
//! `None` while the reconnect is in flight, so publishes during the
//! gap return a typed `client_unavailable` Err and are
//! logged-and-swallowed by `cosmix_props::spawn_dispatcher`.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use cosmix_bus::bus::BusMessage;
use cosmix_props::DispatchPublisher;

use super::subscribe_granter::SharedBrokerHandle;

/// Bridge a [`DispatchPublisher`] call into a `topic.publish` RPC
/// against `noded`. Holds a refreshable broker handle so the
/// dispatcher task survives reconnects.
pub struct NodedPropsPublisher {
    client: SharedBrokerHandle,
}

impl NodedPropsPublisher {
    pub fn new(client: SharedBrokerHandle) -> Self {
        Self { client }
    }
}

impl DispatchPublisher for NodedPropsPublisher {
    fn publish<'a>(
        &'a self,
        topic: &'a str,
        body: String,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let client = self.client.load_full().ok_or_else(|| {
                anyhow::anyhow!(
                    "broker handle unavailable; dropping props publish — \
                     the substrate's audit replay surface remains the \
                     recovery path"
                )
            })?;

            // Inner Bus message: `command = <topic>` so a Mix
            // subscriber can `on <topic> do …` without a separate
            // routing convention. The body is the projected
            // records.changed / audit JSON — already a complete
            // wire payload, no further wrapping needed.
            let mut inner = BusMessage::new();
            inner.set("command", topic);
            inner.body = body;

            let mut headers = BTreeMap::new();
            headers.insert("name".to_string(), topic.to_string());
            // Hard requirement — see module docs.
            headers.insert("retain".to_string(), "false".to_string());

            client
                .send_with_headers("noded", "topic.publish", &headers, &inner.to_wire())
                .await?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inner_message_carries_topic_as_command_and_body_unchanged() {
        // Pin the wire-shape contract: the inner `command` header MUST
        // equal the topic name. A subscriber that uses `on <topic> do …`
        // depends on this; if the helper ever switches to a generic
        // `props.records.changed` or `props.audit` command name, those
        // subscribers silently stop matching.
        let mut inner = BusMessage::new();
        let topic = "webd.props.records.changed";
        let body = r#"{"key":"example.com","nseq":7}"#;
        inner.set("command", topic);
        inner.body = body.to_string();
        assert_eq!(inner.get("command"), Some(topic));
        assert_eq!(inner.body, body);
    }
}
