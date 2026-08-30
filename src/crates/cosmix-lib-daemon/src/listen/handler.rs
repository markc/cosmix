//! The post-accept handler abstraction. The accept driver does the
//! cross-cutting work (bind, accept, guard, optional TLS termination,
//! connection tagging); the daemon supplies a [`ConnHandler`] for what
//! happens *after* — hyper for webd, an SMTP/IMAP session for maild —
//! through one signature.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::TcpStream;

#[cfg(feature = "tls")]
use crate::listen::tls::ListenerTls;

/// A stream handed to a [`ConnHandler`].
///
/// `Plain` and `StartTlsPassthrough` listeners yield [`Tcp`]; a
/// `Terminate` listener yields [`Tls`] (the handshake already
/// completed, so [`ConnCtx::sni`] is populated).
///
/// [`Tcp`]: AcceptedStream::Tcp
/// [`Tls`]: AcceptedStream::Tls
#[non_exhaustive]
pub enum AcceptedStream {
    /// Raw TCP — plain listener, or passthrough where the handler
    /// drives STARTTLS itself.
    Tcp(TcpStream),
    /// A TLS-terminated stream (boxed: it is much larger than the
    /// `Tcp` variant).
    #[cfg(feature = "tls")]
    Tls(Box<tokio_rustls::server::TlsStream<TcpStream>>),
}

/// What a handler learns about a connection — protocol-agnostic.
#[derive(Clone)]
pub struct ConnCtx {
    /// The id of the [`ListenerSpec`] this connection arrived on.
    ///
    /// [`ListenerSpec`]: crate::listen::ListenerSpec
    pub listener_id: Arc<str>,
    /// The local interface address the connection landed on (which
    /// bound IP, for a multi-home listener).
    pub local_addr: SocketAddr,
    /// The remote peer.
    pub peer_addr: SocketAddr,
    /// Whether this listener is public-facing.
    pub external: bool,
    /// The SNI the client sent — `Some` only after a completed
    /// `Terminate` handshake.
    pub sni: Option<String>,
    /// The listener's TLS material, for a passthrough handler to
    /// drive a STARTTLS upgrade or pick a greeting host. `None` on a
    /// plain listener.
    #[cfg(feature = "tls")]
    pub tls: Option<ListenerTls>,
}

/// Per-connection handler. Errors are the handler's to log — the
/// accept driver spawns each connection detached and does not inspect
/// a result (mirrors webd's `let _ = serve_connection(..)` and
/// maild's per-connection spawn).
#[async_trait::async_trait]
pub trait ConnHandler: Send + Sync + 'static {
    async fn handle(&self, stream: AcceptedStream, ctx: ConnCtx);
}
