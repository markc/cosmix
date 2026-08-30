//! SPEC 12 §15.5 + C10 — watch-mediated subscriber bridge trait.
//!
//! The owning service's `<svc>.props.watch` and
//! `<svc>.props.audit.watch` handlers perform the capability check and
//! then ask noded to subscribe the original caller to the reserved
//! `<svc>.props.records.changed` / `<svc>.props.audit` topic — with a
//! `namespace` body-filter so the granted subscription only receives
//! events for the namespace the cap check authorised. The bridge
//! between the in-process handler and the broker subscription is this
//! trait.
//!
//! Production implementation wraps a `cosmix_client::NodedClient` and
//! issues the `noded.props.subscribe_grant` Bus verb (C10b). Test
//! implementations (e.g. the recorder used in this crate's mutation
//! tests) just collect `(topic, target_peer, namespace)` tuples and
//! return success without ever leaving the test process.
//!
//! `Send + Sync` + boxed-future mirror the [`crate::DispatchPublisher`]
//! shape so callers can hold the granter behind `Arc<dyn ...>` and pass
//! it across spawn boundaries without static-type coupling.

use std::future::Future;
use std::pin::Pin;

pub trait SubscribeGranter: Send + Sync {
    /// Grant `target_peer` a broker subscription on `topic` filtered
    /// to publishes whose body's top-level `namespace` field equals
    /// `namespace`. Returns `Ok(())` when noded acknowledges with
    /// `rc=0`; any non-zero rc or transport failure maps to `Err`.
    ///
    /// The implementation is responsible for asserting (via noded's
    /// `subscribe_grant` verb) that the calling service is the
    /// reserved-topic owner — the handler itself does not re-check
    /// ownership before calling `grant`. The owner-peer authentication
    /// happens broker-side using the connection-level `peer_id`
    /// established by `noded.register`.
    fn grant<'a>(
        &'a self,
        topic: &'a str,
        target_peer: &'a str,
        namespace: &'a str,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;
}
