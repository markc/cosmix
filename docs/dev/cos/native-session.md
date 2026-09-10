# Native session implementation staging

The S1 foundation implements the wire types in `cosmix-lib-bus` and shares
noded's existing Axum WebSocket handler between transport identities. TCP
registration, D2 admission and response-channel ownership remain unchanged.

The Unix listener stamps kernel-verified local principals and protects their
traffic from tap and observer payload capture. Ping advertises `native-session:1`
when this ingress is active. The listener uses `noded.unix_socket` when configured, otherwise
`cosmix_path(Run)/noded/bus.sock`; system units pin `COSMIX_RUN=%t/cosmix`.
Client endpoint resolution uses an explicit option, the configured key, the
ping-advertised `native-session-endpoint`, otherwise
`/run/cosmix/noded/bus.sock`, independently of client XDG directories.

The contracts remain [BUS-013–017](../../spec/04-bus-wire.md) and
[BROKER-016–025](../../spec/05-broker-topics.md). Session allocation, grants,
proof verification, binding leases and policy enforcement are S2 and later work;
this foundation does not establish a session-bound principal.
Valid bootstrap requests currently receive structured `UNSUPPORTED`; malformed
bootstrap requests are rejected by the strict wire parser. No grants are minted.

## S2 first boundary: reserve the allocation namespace

The BROKER-017 shape `^[tc][0-9a-z]{1,7}-[a-z2-7]{22}$` is now refused by
`noded.register` on every ingress, including brokers without a Unix listener.
Refusal returns `rc:10` with `reserved_name` and preserves any previous
registration on that connection. Leading-zero and overflowing UID lookalikes
are reserved too; the check does not depend on allocation state or interpret
the displayed UID as authority. Neighbouring legacy names remain valid.

The p0i-02 namespace slice exercises TCP and real Unix WebSocket connections,
pre-claim refusal, retained alias authority and forged `from` canonicalisation.
The allocator, records, proof handling, leases, notices and typed client commands
are still pending. No attached identity is exposed before those lifecycle
protections exist. The broker starts with an empty, non-persistent registry;
there is no live profile-activation switch in this boundary.

## Observation and transport boundary

Observation classification follows native senders, native recipients, correlated
responses, bootstrap commands and private property topics. Retained snapshots
store classification separately from publisher attribution; replays preserve
both without consulting a live publisher route. Observe emits metadata with
`payload_omitted: native_session_protected`; tap omits protected frames entirely.
TCP/D2 callers never acquire a Unix principal assertion; local recipient
services receive the verified Unix sender's stamp only over verified Unix
destination transports. TCP/mesh egress strips all principal metadata, including
live topic fan-out and retained replay.

Native traffic is node-local in S1: it is refused at mesh egress because the
legacy mesh wire has no protected-classification propagation contract. This
does not restrict existing TCP/D2 routing or establish cross-node UID trust.

## Client opt-in boundary

Ordinary `NodedClient::connect` and config-layer default helpers stay on TCP.
`NodedClient::connect_unix` explicitly opts into node-local traffic; it takes
`UnixConnectOptions` with a trusted configured `BrokerAccount` (UID and primary
GID, resolved by the application, never assumed to equal the caller).
Endpoint precedence is the explicit option, the supplied `noded.unix_socket`
value, the ping-discovered path, then `/run/cosmix/noded/bus.sock`. Ping supplies
only a locator: it cannot authorise an endpoint or trigger application traffic.
The config crate's opt-in
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

The listener walks ancestry with directory FDs and `openat(O_NOFOLLOW)`, and
uses the held parent through `/proc/self/fd` for bind and cleanup. Socket chmod
targets a pinned inode; only newly created directories receive mode 0755.
Existing non-traversable directories are refused without widening permissions.
This includes single-user development roots beneath a broker-owned 0700 HOME:
the shared BUS-013 ingress contract still requires user-traversable ancestry.
Such setups must configure a protected endpoint outside HOME; the broker logs
the refusal and remains TCP-only. HOME permissions are never widened.
Root and the broker account remain trusted: they can rename a protected directory
or unlink its socket, causing unavailability, but other users cannot redirect
these anchored operations. Listener failure logs its reason and leaves TCP ready,
with neither native extension entry advertised. This applies to all bind failures;
no runtime bind failure is treated as a fatal configuration contradiction.

Protected correlation tombstones retain precision for 15 minutes after completion
or removal, with a fixed 65,536-entry bound. This is not a confidentiality deadline.
Before enqueueing protected traffic, the broker sets a sticky bit on the recipient
connection, shared across its aliases. That connection's responses remain protected
for its entire lifetime, including duplicates and orphans after tombstone expiry;
no clock clears the bit. This conservatively covers correlated responses too.
Only connection teardown discards it; pure-legacy connections never set it.
Cache overflow also conservatively omits unknown-response payloads during its
horizon. Legacy-only reserved property
topics retain their prior observation behaviour; a topic name alone confers no
native classification.

Term and grant-bearing Mix will require Unix in S3 and generate no
mesh-destined traffic in v1. Unix mesh-egress refusal remains fail-closed;
remote delegation and protected mesh transit are S5 concerns. No API silently
replays a refused mesh request over TCP.
