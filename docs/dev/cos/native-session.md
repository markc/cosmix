# Native session implementation staging

The S1 foundation implements the wire types in `cosmix-lib-bus` and shares
noded's existing Axum WebSocket handler between transport identities. TCP
registration, D2 admission and response-channel ownership remain unchanged.

The Unix listener stamps kernel-verified local principals and protects their
traffic from tap and observer payload capture. Ping advertises `native-session:1`
when this ingress is active. The listener uses `noded.unix_socket` when configured, otherwise
`cosmix_path(Run)/noded/bus.sock`; system units pin `COSMIX_RUN=%t/cosmix`.
Client endpoint resolution uses the configured key, otherwise
`/run/cosmix/noded/bus.sock`, independently of client XDG directories.

The contracts remain [BUS-013–017](../../spec/04-bus-wire.md) and
[BROKER-016–025](../../spec/05-broker-topics.md). Session allocation, grants,
proof verification, binding leases and policy enforcement are S2 and later work;
this foundation does not establish a session-bound principal.
Valid bootstrap requests currently receive structured `UNSUPPORTED`; malformed
bootstrap requests are rejected by the strict wire parser. No grants are minted.

Observation classification follows native senders, native recipients, correlated
responses, bootstrap commands and private property topics. Retained snapshots
store classification separately from publisher attribution; replays preserve
both without consulting a live publisher route. Observe emits metadata with
`payload_omitted: native_session_protected`; tap omits protected frames entirely.
TCP/D2 callers never acquire a Unix principal assertion; local recipient
services receive the verified Unix sender's stamp regardless of their transport.

Native traffic is node-local in S1: it is refused at mesh egress because the
legacy mesh wire has no protected-classification propagation contract. This
does not restrict existing TCP/D2 routing or establish cross-node UID trust.

## Client opt-in boundary

Ordinary `NodedClient::connect` and config-layer default helpers stay on TCP.
`NodedClient::connect_unix` explicitly opts into node-local traffic; it takes
`UnixConnectOptions` with a trusted configured `BrokerAccount` (UID and primary
GID, resolved by the application, never assumed to equal the caller).
Endpoint precedence is the explicit option, the supplied `noded.unix_socket`
value, then `/run/cosmix/noded/bus.sock`. The config crate's opt-in
`client_helpers::unix_connect_options` supplies that config value without a
reverse dependency from lib-client.

The client checks all path components and socket ownership, verifies server
SO_PEERCRED, checks the path again for replacement, then negotiates version 1.
Symlinks and writable socket directories are refused, including development
endpoints. Root-owned sticky ancestors may contain a protected broker-owned
directory. Neither root nor the configured broker account is an adversary in
this endpoint-authentication boundary. Credentials are connect-time snapshots.

`require_native_session` forbids every TCP downgrade, including when fallback
was separately enabled. Ordinary Unix opt-in can explicitly enable fallback;
the result is then `UnixConnectOutcome::UnverifiedTcp`, with the Unix failure
retained. The successful Unix result is `VerifiedUnix`. Only its `recv` method
creates immutable `VerifiedCommand` deliveries with a `trusted_context`
accessor; raw `IncomingCommand` headers cannot create this type. A missing
context denotes an unverified sender or a direct broker message. A typed stamp
is not a live lease: retained deliveries remain historical, and session-bound
authorisation will require S2 lease checks. Use `client().close()` for explicit
connection teardown, as with the existing client.

Term and grant-bearing Mix will require Unix in S3 and generate no
mesh-destined traffic in v1. Unix mesh-egress refusal remains fail-closed;
remote delegation and protected mesh transit are S5 concerns. No API silently
replays a refused mesh request over TCP.
