//! The `Adapter` trait — one domain, one Bus service, one task — and the
//! context a run receives.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Result;
#[cfg(feature = "cosmix")]
use anyhow::anyhow;
use tokio::sync::{oneshot, watch};

/// An adapter's run future, as the object-safe trait hands it out.
pub type BoxRunFuture = Pin<Box<dyn Future<Output = Result<()>> + Send>>;

/// The desktop session bus the adapters dial, resolved once per process
/// from `DBUS_SESSION_BUS_ADDRESS`.
///
/// `Unavailable` carries the human-readable reason (the environment
/// variable is unset or empty); an address that is set but unreachable
/// surfaces at dial time as an ordinary adapter failure and lands the
/// adapter in backoff with that reason — the daemon itself stays up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionBus {
    Address(String),
    Unavailable(String),
}

impl SessionBus {
    /// Resolve from an optional `DBUS_SESSION_BUS_ADDRESS` value. Pure,
    /// so the unset/empty cases are testable without touching the
    /// process environment.
    pub fn from_address_value(value: Option<String>) -> Self {
        match value {
            Some(address) if !address.trim().is_empty() => Self::Address(address),
            Some(_) => Self::Unavailable("DBUS_SESSION_BUS_ADDRESS is set but empty".into()),
            None => Self::Unavailable("DBUS_SESSION_BUS_ADDRESS is not set".into()),
        }
    }

    /// Resolve from the process environment.
    pub fn from_env() -> Self {
        Self::from_address_value(std::env::var("DBUS_SESSION_BUS_ADDRESS").ok())
    }

    /// The address, or the reason it is missing.
    pub fn address(&self) -> std::result::Result<&str, &str> {
        match self {
            Self::Address(address) => Ok(address),
            Self::Unavailable(reason) => Err(reason),
        }
    }
}

/// A D-Bus domain adapter. One adapter owns one Bus service (registered
/// under [`Adapter::bus_service`]) and one zbus connection for its whole
/// run; both are locals of `run`, so returning, erroring or panicking
/// drops them and withdraws the names — fault containment by ownership.
///
/// `run` must return only when the adapter is done for: `Ok` counts as
/// an unexpected exit and `Err` as a failure; both put the adapter into
/// backoff and restart it. A panic is caught the same way via the
/// JoinHandle. To stop on daemon shutdown, await `ctx.shutdown()`.
pub trait Adapter: Send + 'static {
    /// Adapter name: config key and props path segment (e.g. `notify`).
    fn name(&self) -> &'static str;
    /// The Bus service name this adapter registers under (its domain).
    fn bus_service(&self) -> &'static str;
    /// Run the adapter until `ctx.shutdown()` fires or it fails.
    fn run(self: Box<Self>, ctx: AdapterCtx) -> BoxRunFuture;
}

/// How to build a fresh adapter. The supervisor re-creates the adapter
/// on every (re)start, so the registry holds factories, not instances.
pub type AdapterFactory = Arc<dyn Fn() -> Box<dyn Adapter> + Send + Sync>;

/// One registry entry: everything the supervisor needs to launch an
/// adapter by name. Adding a built-in adapter is one
/// [`crate::adapter_spec`] line in `citizen::builtin_adapters`.
#[derive(Clone)]
pub struct AdapterSpec {
    pub name: String,
    pub service: String,
    pub factory: AdapterFactory,
}

/// Register a built-in adapter: `adapter_spec::<NotifyAdapter>()`.
pub fn adapter_spec<A>() -> AdapterSpec
where
    A: Adapter + Default,
{
    let probe = A::default();
    AdapterSpec {
        name: probe.name().to_string(),
        service: probe.bus_service().to_string(),
        factory: Arc::new(|| Box::new(A::default())),
    }
}

/// Bus identity an adapter needs to register its domain service:
/// the service name plus (under the `cosmix` feature) the daemon's
/// build provenance, re-sent on every registration.
#[derive(Debug, Clone)]
pub struct BusIdentity {
    pub service: String,
    #[cfg(feature = "cosmix")]
    pub provenance: cosmix_bus::RegisterProvenance,
}

/// What a run receives: the session-bus endpoint, this launch's stop
/// signal, a one-shot ready channel, and (when the daemon provides it)
/// the Bus identity to register under.
///
/// `shutdown` is per launch, not per daemon: the supervisor flips it on
/// daemon shutdown, on `dbusd.adapter.disable`, and on
/// `dbusd.adapter.restart` — an adapter that only ever awaits it stops
/// correctly for all three.
pub struct AdapterCtx {
    session_bus: SessionBus,
    shutdown: watch::Receiver<bool>,
    ready: Option<oneshot::Sender<()>>,
    bus_identity: Option<BusIdentity>,
}

impl AdapterCtx {
    pub fn new(session_bus: SessionBus, shutdown: watch::Receiver<bool>) -> Self {
        Self {
            session_bus,
            shutdown,
            ready: None,
            bus_identity: None,
        }
    }

    /// Attach this launch's ready sender (supervisor-side).
    pub fn set_ready(&mut self, ready: oneshot::Sender<()>) {
        self.ready = Some(ready);
    }

    /// Attach the Bus identity for `connect_bus` (daemon-side).
    pub fn set_bus_identity(&mut self, identity: BusIdentity) {
        self.bus_identity = Some(identity);
    }

    /// The resolved session-bus endpoint (address, or why there is none).
    pub fn session_bus(&self) -> &SessionBus {
        &self.session_bus
    }

    /// This launch's stop signal: `true` means stop now, return, drop
    /// every connection you own.
    pub fn shutdown(&self) -> &watch::Receiver<bool> {
        &self.shutdown
    }

    /// Report that the adapter is serving (names acquired). Moves the
    /// supervision state from `starting` to `running`. Optional: an
    /// adapter that never signals stays `starting` while it runs.
    pub fn signal_ready(&mut self) {
        if let Some(ready) = self.ready.take() {
            let _ = ready.send(());
        }
    }

    /// Open this adapter's own Bus connection, registered under its
    /// domain service name. The returned client is a local of `run` —
    /// dropping it withdraws the service.
    #[cfg(feature = "cosmix")]
    pub async fn connect_bus(&self) -> Result<cosmix_client::NodedClient> {
        let identity = self
            .bus_identity
            .as_ref()
            .ok_or_else(|| anyhow!("adapter context carries no Bus identity"))?;
        cosmix_config::client_helpers::connect_default_with_provenance(
            &identity.service,
            identity.provenance.clone(),
        )
        .await
    }

    /// Dial the desktop session bus at the resolved
    /// `DBUS_SESSION_BUS_ADDRESS`. Fails with the missing-bus reason
    /// when the variable is unset; an unreachable address fails here
    /// too, which the supervisor treats as an ordinary adapter failure.
    #[cfg(feature = "cosmix")]
    pub async fn connect_session_bus(&self) -> Result<zbus::Connection> {
        let address = match self.session_bus.address() {
            Ok(address) => address,
            Err(reason) => return Err(anyhow!("session bus unavailable: {reason}")),
        };
        let address: zbus::address::Address = address
            .parse()
            .map_err(|error| anyhow!("invalid session bus address {address:?}: {error}"))?;
        zbus::connection::Builder::address(address)?
            .build()
            .await
            .map_err(|error| anyhow!("session bus dial failed: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_bus_resolution_covers_set_unset_and_empty() {
        assert_eq!(
            SessionBus::from_address_value(Some("unix:path=/run/bus".into())),
            SessionBus::Address("unix:path=/run/bus".into())
        );
        assert_eq!(
            SessionBus::from_address_value(Some("  ".into())),
            SessionBus::Unavailable("DBUS_SESSION_BUS_ADDRESS is set but empty".into())
        );
        let unavailable = SessionBus::from_address_value(None);
        assert_eq!(
            unavailable.address(),
            Err("DBUS_SESSION_BUS_ADDRESS is not set")
        );
    }
}
