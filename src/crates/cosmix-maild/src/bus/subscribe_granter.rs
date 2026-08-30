//! Production [`SubscribeGranter`] for maild — bridges
//! `<svc>.props.watch` / `<svc>.props.audit.watch` to noded's
//! `noded.props.subscribe_grant` verb (SPEC 12 §15.5, C10b).
//!
//! ## Refreshable client handle
//!
//! `PropsRouter` is built at startup *before* Bus `run()` can register
//! with noded, and maild's Bus task now reconnects with backoff after
//! initial failures or mid-life stream ends. A one-shot client slot
//! would keep using the first, closed [`NodedClient`] after a broker
//! disconnect and leave watch grants broken until process restart.
//!
//! Maild therefore shares an `Arc<ArcSwapOption<NodedClient>>` between
//! the granter, the props publisher, and the verdict publisher.
//! `bus::run` swaps in the freshly connected client on every successful
//! registration and stores `None` while a reconnect is in flight. The
//! granter loads the current client per `grant()` call; when the handle
//! is empty the watch handler maps the typed error to
//! `rc=10 grant_failed`, and the peer can retry once the broker is back.
//!
//! ## Known limitation — timeout divergence
//!
//! `call_with_headers` has a 60s response timeout. If the request
//! reaches noded but the response is lost (broker stall, WS hiccup),
//! the granter surfaces `grant_failed` to the watching peer *while*
//! the broker may have successfully installed the subscription. The
//! peer thus sees "watch failed" but may start receiving live events.
//! This does NOT explode subscription count because noded's
//! `subscribe_topic_filtered` is idempotent on `(peer, topic, filter)`
//! — a peer retry returns the same subscription_id. The divergence is
//! a UX wart, not a correctness bug; closing it cleanly requires a
//! revoke path or a granter-side ack-and-confirm protocol, neither of
//! which is in scope for C10.
//!
//! ## No revoke path (yet)
//!
//! `grant()` discards the broker's `subscription_id` because the watch
//! handler doesn't expose a peer-facing revoke verb. Subscriptions
//! survive until the target peer disconnects (broker drops the entry
//! on close). A future C11 (or later) can surface a revoke pathway by
//! threading the id back through `SubscribeGranter` — until then
//! noded's idempotency on re-grant keeps the registry bounded.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use arc_swap::ArcSwapOption;
use cosmix_client::NodedClient;
use cosmix_props::subscribe_granter::SubscribeGranter;

/// Shared, refreshable broker handle. Cloned cheaply into the granter
/// and publishers, updated by `bus::run` on every successful
/// connect/disconnect.
pub type SharedBrokerHandle = Arc<ArcSwapOption<NodedClient>>;

/// Build a fresh empty broker handle. Used during maild startup before
/// either the granter or the long-lived publisher tasks are created.
pub fn new_broker_handle() -> SharedBrokerHandle {
    Arc::new(ArcSwapOption::empty())
}

pub struct NodedSubscribeGranter {
    client: SharedBrokerHandle,
}

impl Default for NodedSubscribeGranter {
    fn default() -> Self {
        Self::new(new_broker_handle())
    }
}

impl NodedSubscribeGranter {
    pub fn new(client: SharedBrokerHandle) -> Self {
        Self { client }
    }

    /// Clone the shared broker handle for long-lived publisher tasks.
    pub fn broker_handle(&self) -> SharedBrokerHandle {
        self.client.clone()
    }

    /// Refresh the [`NodedClient`] used to issue
    /// `noded.props.subscribe_grant` RPCs. Called by `bus::run` on
    /// every successful broker registration; multiple calls are
    /// expected across reconnects.
    pub fn install_client(&self, client: Arc<NodedClient>) {
        self.client.store(Some(client));
    }

    /// Clear the broker handle while a reconnect is in flight so
    /// grants fail fast instead of calling into a closed client.
    pub fn clear_client(&self) {
        self.client.store(None);
    }
}

impl SubscribeGranter for NodedSubscribeGranter {
    fn grant<'a>(
        &'a self,
        topic: &'a str,
        target_peer: &'a str,
        namespace: &'a str,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let client = self.client.load_full().ok_or_else(|| {
                anyhow::anyhow!(
                    "NodedClient not yet installed on NodedSubscribeGranter — \
                     broker registration may still be pending or has failed"
                )
            })?;

            let mut headers = BTreeMap::new();
            headers.insert("topic".to_string(), topic.to_string());
            headers.insert("target_peer".to_string(), target_peer.to_string());
            headers.insert("namespace".to_string(), namespace.to_string());

            // `call_with_headers` awaits the rc/body response and maps
            // rc>=10 to Err. A rc=0 body of
            //   {"subscription_id":"...","namespace":"..."}
            // confirms the broker has installed the per-namespace
            // body-filtered subscription against `target_peer`. We
            // don't surface the subscription_id upstream — the watch
            // handler doesn't (yet) expose a revoke path; this lands
            // with the wider lifecycle story.
            let _resp = client
                .call_with_headers("noded", "noded.props.subscribe_grant", &headers, "")
                .await?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn grant_before_install_returns_err_not_panic() {
        // Load-bearing invariant: a watch verb arriving before the
        // broker connection is up must surface a *typed* error to the
        // PropsRouter, which then projects rc=10 grant_failed. An
        // unwrap/expect or OnceCell::get panic here would crash the
        // dispatcher task and silently disable the watch surface.
        let granter = NodedSubscribeGranter::new(new_broker_handle());
        let err = granter
            .grant("maild.props.records.changed", "peer", "accounts")
            .await
            .expect_err("uninstalled granter must return Err");
        assert!(
            err.to_string().contains("NodedClient not yet installed"),
            "error must identify the install gap, got: {err}",
        );
    }

    #[tokio::test]
    async fn clear_client_on_already_empty_handle_is_idempotent() {
        // clear_client() must be safe to call even when no client was ever
        // installed (e.g. a connect attempt fails before install_client()
        // runs) — it must not panic and must leave the install-gap error
        // in place rather than e.g. poisoning the handle.
        let handle = new_broker_handle();
        let granter = NodedSubscribeGranter::new(handle);
        granter.clear_client();
        let err = granter
            .grant("maild.props.records.changed", "peer", "accounts")
            .await
            .expect_err("cleared granter must return Err");
        assert!(err.to_string().contains("NodedClient not yet installed"));
    }
}
