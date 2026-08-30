//! Production [`SubscribeGranter`] for webd — bridges
//! `<svc>.props.watch` / `<svc>.props.audit.watch` to noded's
//! `noded.props.subscribe_grant` verb (SPEC 12 §15.5, C10b).
//!
//! ## Refreshable client handle (webd's reconnect-loop variant)
//!
//! Maild's equivalent module pins the [`NodedClient`] in a
//! [`tokio::sync::OnceCell`] because maild's `bus::run` connects
//! exactly once and exits if the broker drops. Webd's `bus::run`
//! has a retry-with-backoff reconnect loop, so a OnceCell-pinned
//! client would survive the first broker disconnect as a closed
//! handle — every subsequent `grant()` would call
//! `call_with_headers` on a socket the broker had already reaped.
//! That's the "MAJOR" finding Codex caught against the first
//! draft of this file.
//!
//! Webd therefore shares an `Arc<ArcSwapOption<NodedClient>>`
//! between the granter and the
//! [`NodedPropsPublisher`](super::props_publisher::NodedPropsPublisher).
//! `bus::run` swaps in the freshly-connected client on every
//! successful registration (and stores `None` while a reconnect
//! is in flight, so publishes fail fast instead of against a
//! closed Arc). The granter loads the current client per `grant`
//! call; if the handle is `None` we surface a typed error that
//! the watch handler projects to `rc=10 grant_failed`.
//!
//! ## No revoke path (yet)
//!
//! `grant()` discards the broker's `subscription_id` because the
//! watch handler doesn't expose a peer-facing revoke verb.
//! Subscriptions survive until the target peer disconnects.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use arc_swap::ArcSwapOption;
use cosmix_client::NodedClient;
use cosmix_props::subscribe_granter::SubscribeGranter;

/// Shared, refreshable broker handle. Cloned (cheap — it's an
/// `Arc<ArcSwapOption<_>>`) into the granter and the publisher,
/// updated by `bus::run` on every successful connect/disconnect.
pub type SharedBrokerHandle = Arc<ArcSwapOption<NodedClient>>;

/// Build a fresh empty broker handle. Used by `main.rs` to construct
/// the shared cell before either the granter or the publisher is
/// created.
pub fn new_broker_handle() -> SharedBrokerHandle {
    Arc::new(ArcSwapOption::empty())
}

pub struct NodedSubscribeGranter {
    client: SharedBrokerHandle,
}

impl NodedSubscribeGranter {
    pub fn new(client: SharedBrokerHandle) -> Self {
        Self { client }
    }

    /// Refresh the broker handle. `bus::run` calls this with `Some`
    /// on every successful registration and (optionally) with `None`
    /// while a reconnect is in flight. Multiple calls are expected —
    /// this is the reconnect-aware variant of maild's one-shot
    /// `install_client`.
    pub fn install_client(&self, client: Arc<NodedClient>) {
        self.client.store(Some(client));
    }

    /// Clear the broker handle. After this returns, `grant()` calls
    /// surface a typed `client_unavailable` error until the next
    /// `install_client`. Exists so `bus::run` can fail watch verbs
    /// fast across the reconnect-gap rather than calling into a
    /// closed `NodedClient` Arc and waiting for the underlying I/O
    /// timeout.
    #[allow(dead_code)]
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
        // unwrap/expect would crash the dispatcher task and silently
        // disable the watch surface.
        let granter = NodedSubscribeGranter::new(new_broker_handle());
        let err = granter
            .grant("webd.props.records.changed", "peer", "vhosts")
            .await
            .expect_err("uninstalled granter must return Err");
        assert!(
            err.to_string().contains("NodedClient not yet installed"),
            "error must identify the install gap, got: {err}",
        );
    }

    #[tokio::test]
    async fn clear_after_install_re_arms_install_gap_error() {
        // `clear_client` must restore the "not yet installed" posture
        // so a `grant()` between broker reconnects fails fast with
        // the typed error rather than racing a stale Arc to the wire.
        let handle = new_broker_handle();
        let granter = NodedSubscribeGranter::new(handle.clone());
        // No client yet — confirm baseline.
        let err = granter
            .grant("webd.props.records.changed", "peer", "vhosts")
            .await
            .expect_err("baseline: no client => Err");
        assert!(err.to_string().contains("not yet installed"));
        // Clear is idempotent and survives a never-installed handle.
        granter.clear_client();
        let err = granter
            .grant("webd.props.records.changed", "peer", "vhosts")
            .await
            .expect_err("post-clear: no client => Err");
        assert!(err.to_string().contains("not yet installed"));
    }
}
