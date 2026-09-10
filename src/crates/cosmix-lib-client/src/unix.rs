//! Explicit, node-local BUS-013 transport. Ordinary `NodedClient::connect`
//! remains TCP. Unix traffic cannot leave this node in v1; remote delegation
//! and protected mesh transit are S5 work.

use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Component, Path, PathBuf};

use cosmix_bus::native_session::BrokerPrincipal;
use tokio::sync::mpsc;

use crate::{IncomingCommand, NodedClient};

/// Trusted configuration, resolved by the application from its broker account.
/// No fixed numeric UID or caller-UID default is assumed.
#[derive(Debug, Clone, Copy)]
pub struct BrokerAccount {
    pub uid: u32,
    pub gid: u32,
}

/// Explicit Unix opt-in. Supply the node-config value from the cos layer;
/// this crate deliberately has no dependency on cosmix-lib-config.
#[derive(Debug, Clone)]
pub struct UnixConnectOptions {
    pub broker_account: BrokerAccount,
    pub endpoint: Option<PathBuf>,
    pub configured_endpoint: Option<PathBuf>,
    /// Hard requirement. Overrides `allow_unverified_tcp_fallback` on every
    /// failure, including permissions, ownership, peer credentials and version.
    pub require_native_session: bool,
    /// Explicitly allow a fresh TCP connection after Unix setup fails. It
    /// never carries trusted context, even if TCP advertises native-session.
    pub allow_unverified_tcp_fallback: bool,
}

impl UnixConnectOptions {
    pub fn new(broker_account: BrokerAccount) -> Self {
        Self {
            broker_account,
            endpoint: None,
            configured_endpoint: None,
            require_native_session: false,
            allow_unverified_tcp_fallback: false,
        }
    }

    /// Explicit endpoint, then `noded.unix_socket`, then the system endpoint.
    /// Never consults XDG_RUNTIME_DIR, COSMIX_RUN or the caller's home.
    pub fn resolved_endpoint(&self) -> &Path {
        self.endpoint
            .as_deref()
            .or(self.configured_endpoint.as_deref())
            .unwrap_or_else(|| Path::new("/run/cosmix/noded/bus.sock"))
    }
}

/// Structured connection failure. Diagnostics never include wire payloads.
#[derive(Debug)]
pub enum ConnectError {
    InvalidEndpoint,
    EndpointOwnership,
    EndpointChanged,
    PeerCredentials,
    UnsupportedVersion,
    Io(std::io::Error),
    Protocol(anyhow::Error),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidEndpoint => {
                "Unix endpoint must be absolute without symlinks or dot components"
            }
            Self::EndpointOwnership => "Unix endpoint or ancestor ownership/mode is not protected",
            Self::EndpointChanged => "Unix endpoint changed during connection verification",
            Self::PeerCredentials => {
                "Unix server credentials do not match configured broker account"
            }
            Self::UnsupportedVersion => "broker does not support native-session version 1",
            Self::Io(_) => "Unix endpoint I/O failed",
            Self::Protocol(_) => "broker connection setup failed",
        })
    }
}
impl std::error::Error for ConnectError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Protocol(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}

/// The fallback is deliberately a distinct, explicitly unverified variant.
/// Neither raw headers nor an extension advertisement can upgrade it.
pub enum UnixConnectOutcome {
    VerifiedUnix(VerifiedConnection),
    UnverifiedTcp {
        client: NodedClient,
        unix_error: ConnectError,
    },
}

/// Only endpoint verification plus profile negotiation can construct this.
pub struct VerifiedConnection {
    client: NodedClient,
    incoming: mpsc::UnboundedReceiver<VerifiedCommand>,
}
impl VerifiedConnection {
    /// Requests/replies use the existing ABP client API. Its raw receive lane
    /// is absent on this handle; trusted deliveries come only from `recv`.
    pub fn client(&self) -> &NodedClient {
        &self.client
    }

    pub async fn recv(&mut self) -> Option<VerifiedCommand> {
        self.incoming.recv().await
    }
}

/// Immutable delivery paired with context parsed on its verified transport.
/// No public constructor, deserialiser or mutable command accessor exists.
/// Context authenticates the stamp, not a current session lease: cached and
/// retained deliveries still require BROKER-020/PROP-025 lease enforcement.
///
/// ```compile_fail
/// fn forge(command: cosmix_client::IncomingCommand) {
///     let _ = cosmix_client::VerifiedCommand::new(command, None);
/// }
/// ```
/// ```compile_fail
/// fn trust_raw(command: cosmix_client::IncomingCommand) {
///     let _ = command.trusted_context();
/// }
/// ```
pub struct VerifiedCommand {
    command: IncomingCommand,
    principal: Option<BrokerPrincipal>,
}
impl VerifiedCommand {
    pub(crate) fn new(command: IncomingCommand, principal: Option<BrokerPrincipal>) -> Self {
        Self { command, principal }
    }
    pub fn command(&self) -> &IncomingCommand {
        &self.command
    }
    /// None for unverified senders and direct broker control messages.
    pub fn trusted_context(&self) -> Option<&BrokerPrincipal> {
        self.principal.as_ref()
    }
}

impl NodedClient {
    /// Explicit Unix opt-in; ordinary `connect` remains unchanged. Native
    /// session clients MUST set `require_native_session`. Unix is node-local
    /// in v1: mesh-destined traffic is refused, with no transparent TCP retry.
    pub async fn connect_unix(
        service_name: &str,
        tcp_url: &str,
        options: &UnixConnectOptions,
        provenance: Option<cosmix_bus::RegisterProvenance>,
    ) -> Result<UnixConnectOutcome, ConnectError> {
        match connect_verified(service_name, options, provenance.clone()).await {
            Ok(connection) => Ok(UnixConnectOutcome::VerifiedUnix(connection)),
            Err(unix_error)
                if !options.require_native_session && options.allow_unverified_tcp_fallback =>
            {
                let client = Self::connect_with_provenance(service_name, tcp_url, provenance)
                    .await
                    .map_err(ConnectError::Protocol)?;
                Ok(UnixConnectOutcome::UnverifiedTcp { client, unix_error })
            }
            Err(error) => Err(error),
        }
    }
}

async fn connect_verified(
    service_name: &str,
    options: &UnixConnectOptions,
    provenance: Option<cosmix_bus::RegisterProvenance>,
) -> Result<VerifiedConnection, ConnectError> {
    let path = options.resolved_endpoint();
    let before = verify_path(path, options.broker_account)?;
    let socket = tokio::net::UnixStream::connect(path)
        .await
        .map_err(ConnectError::Io)?;
    // SO_PEERCRED describes the server at connect time, not later setuid.
    let peer = socket.peer_cred().map_err(ConnectError::Io)?;
    if peer.uid() != options.broker_account.uid || peer.gid() != options.broker_account.gid {
        return Err(ConnectError::PeerCredentials);
    }
    if verify_path(path, options.broker_account)? != before {
        return Err(ConnectError::EndpointChanged);
    }
    let (ws, _) = tokio_tungstenite::client_async("ws://localhost/ws", socket)
        .await
        .map_err(|error| ConnectError::Protocol(error.into()))?;
    let (client, incoming) = NodedClient::from_verified_unix(ws, service_name, provenance)
        .await
        .map_err(|error| match error.downcast::<ConnectError>() {
            Ok(error) => error,
            Err(error) => ConnectError::Protocol(error),
        })?;
    Ok(VerifiedConnection { client, incoming })
}

/// Check every component, rejecting symlinks and unprotected replacement
/// paths. A root-owned sticky ancestor (e.g. /tmp) is allowed only above a
/// protected broker/root-owned child. Sticky rules protect that child from
/// other UIDs. The immediate socket directory must never be world-writable.
type PathSnapshot = Vec<(u64, u64, u32, u32, u32)>;

fn verify_path(path: &Path, account: BrokerAccount) -> Result<PathSnapshot, ConnectError> {
    if !path.is_absolute()
        || path
            .as_os_str()
            .as_encoded_bytes()
            .split(|b| *b == b'/')
            .any(|part| part == b"." || part == b"..")
    {
        return Err(ConnectError::InvalidEndpoint);
    }
    let parent = path.parent().ok_or(ConnectError::InvalidEndpoint)?;
    let mut current = PathBuf::new();
    let mut identities = Vec::new();
    for component in path.components() {
        if !matches!(component, Component::RootDir | Component::Normal(_)) {
            return Err(ConnectError::InvalidEndpoint);
        }
        current.push(component);
        let meta = std::fs::symlink_metadata(&current).map_err(ConnectError::Io)?;
        if current == path {
            if !meta.file_type().is_socket() || meta.uid() != account.uid {
                return Err(ConnectError::EndpointOwnership);
            }
        } else {
            let sticky_ancestor = current != parent && meta.uid() == 0 && meta.mode() & 0o1000 != 0;
            if !meta.is_dir()
                || (meta.uid() != 0 && meta.uid() != account.uid)
                || (meta.mode() & 0o022 != 0 && !sticky_ancestor)
            {
                return Err(ConnectError::EndpointOwnership);
            }
        }
        identities.push((meta.dev(), meta.ino(), meta.uid(), meta.gid(), meta.mode()));
    }
    Ok(identities)
}

// Scan before the compatibility parser can overwrite identical header keys.
// BUS-014 allows only one canonical spelling. JSON duplicates and known-field
// types are then checked by lib-bus read_principal.
pub(crate) fn principal_header_is_unique(wire: &str) -> bool {
    let Some(content) = wire.strip_prefix("---\n") else {
        return false;
    };
    // Match the compatibility parser's boundary, including unterminated
    // headers. The opening delimiter must not terminate the scan.
    let headers = content
        .split_once("\n---\n")
        .map_or(content, |(headers, _)| headers);
    let mut seen = false;
    for line in headers.lines() {
        if let Some((key, _)) = line.split_once(':')
            && key.trim().eq_ignore_ascii_case("broker_principal")
        {
            if seen || key != "broker_principal" {
                return false;
            }
            seen = true;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn endpoint_resolution_is_explicit_config_system_never_user_runtime() {
        let mut options = UnixConnectOptions::new(BrokerAccount { uid: 123, gid: 123 });
        assert_eq!(
            options.resolved_endpoint(),
            Path::new("/run/cosmix/noded/bus.sock")
        );
        options.configured_endpoint = Some("/run/example/config.sock".into());
        assert_eq!(
            options.resolved_endpoint(),
            Path::new("/run/example/config.sock")
        );
        options.endpoint = Some("/run/example/explicit.sock".into());
        assert_eq!(
            options.resolved_endpoint(),
            Path::new("/run/example/explicit.sock")
        );
        assert!(!options.allow_unverified_tcp_fallback);
    }
    #[test]
    fn principal_duplicates_are_detected_before_compatibility_parse() {
        assert!(principal_header_is_unique(
            "---\nbus: 1\nbroker_principal: {}\n---\nbroker_principal: body"
        ));
        for key in ["broker_principal", "BROKER_PRINCIPAL", "Broker_Principal"] {
            assert!(!principal_header_is_unique(&format!(
                "---\nbroker_principal: {{}}\n{key}: {{}}\n---\n"
            )));
        }
        assert!(!principal_header_is_unique(
            "---\nBROKER_PRINCIPAL: {}\n---\n"
        ));
        assert!(!principal_header_is_unique(
            "---\n broker_principal: {}\n---\n"
        ));
        assert!(!principal_header_is_unique(
            "---\nbroker_principal: {}\nbroker_principal: {}"
        ));
        assert!(principal_header_is_unique(
            "---\nbus: 1\n---\nBROKER_PRINCIPAL: body"
        ));
    }
}
