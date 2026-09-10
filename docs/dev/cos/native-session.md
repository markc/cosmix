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
