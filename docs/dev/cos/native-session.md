# Native session implementation staging

The S1 foundation implements the wire types in `cosmix-lib-bus` and shares
noded's existing Axum WebSocket handler between transport identities. TCP
registration, D2 admission and response-channel ownership remain unchanged.

The Unix listener and principal stamping are staged inactive until protected
observation is connected. This intermediate revision does not advertise
`native-session`. The listener uses `noded.unix_socket` when configured, otherwise
`cosmix_path(Run)/noded/bus.sock`; system units pin `COSMIX_RUN=%t/cosmix`.
Client endpoint resolution uses the configured key, otherwise
`/run/cosmix/noded/bus.sock`, independently of client XDG directories.

The contracts remain [BUS-013–017](../../spec/04-bus-wire.md) and
[BROKER-016–025](../../spec/05-broker-topics.md). Session allocation, grants,
proof verification, binding leases and policy enforcement are S2 and later work;
this foundation does not establish a session-bound principal.
