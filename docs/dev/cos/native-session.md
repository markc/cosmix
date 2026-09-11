# Native session implementation staging

## S2 lifecycle delivery boundary

Lifecycle transitions offer metadata-only notices to bindings, attached parents,
key interests and affected recipients. Notice storage is bounded to 256 entries
per connection and 4,096 globally. Overflow sets a separate coalesced gap bit;
global pressure sheds from a largest backlog. The writer sends gaps before queued
notices. Slow readers are never disconnected because of notice overflow.
Replacement attachment offers the new generation to the closing channel as well
as the successor. A channel that has held a binding cannot allocate or prove a
different record while its close is pending; queued bootstrap work preserves
the original binding scope.

Bound routed deliveries register a connection/reference dependency before enqueue,
under the same lock as revocation. Dependency caps are 256 per recipient and 8,192
globally; exhaustion refuses the delivery. `lease.check` requires an existing
unexpired dependency, atomically refreshes its notice lifetime through the returned
lease, and returns the minimum remaining ancestor lease. Recipients
use request-start CLOCK_BOOTTIME plus that delta, never receive time plus delta.

The same fence covers correlated responses and fresh topic fan-out. Retained
topic replay preserves historical attribution and cannot refresh dependencies.
Ping publishes the effective bounds in `native_session_limits` (decimal strings).
`noded.pending_grants_per_parent` configures the per-Term pending-grant cap
(integer, default 32, range 0–32). Zero disables new grants. Values above 32
refuse startup before listener activation; the global 1,024-grant and per-UID
64-Term ceilings still apply. Ping advertises the configured value.

## S2 fixture execution

The native noded tests use real Unix WebSockets and kernel peer credentials.
They include separate-process reserved-name competition, altered-scope and
captured-proof refusal after connection loss and against a new challenge,
fresh resumption, restart/re-enrolment, parent-resume wake, lease dependency
registration, observation/log omission, grant/Term/challenge/interest exhaustion,
retention high-water and deterministic notice-queue overflow. A test-only probe
fills the real broker's notice queue under its lock, then checks gap delivery and
key-selected resynchronisation over the real Unix connection. Clock-boundary
unit tests supplement that transport coverage with exact expiry instants.

The privileged multi-UID case is explicitly ignored in ordinary runs, with its
missing prerequisites named in the test report. Run it explicitly as root with
`COSMIX_SESSION_TEST_UID` set to a distinct unprivileged UID. It uses a real child
process after UID/GID change. Missing prerequisites fail that explicit invocation;
there is no early-return path that reports a skipped security assertion as passed.

## S2 grants and proof boundary

`grant.create` and `grant.fetch` require the issuing attached parent. Grants
reserve generation-qualified pane records, expire after 30 seconds, and consume
only on successful strict Ed25519 proof. Both challenge selectors are supported;
identical outstanding selectors retain their original five-second deadline.
Key lookups register wake interest (256 per UID) independently of lookup success.
At most 128 challenges per UID and one per connection are outstanding. Malformed
identifiable proof attempts consume that connection's challenge, never a grant.
Fresh proof supports enrolment and attached/suspended resumption. Parent terminal
transitions recursively revoke children under the registry/lifecycle lock.

## S2 allocation boundary

The broker implements `hello`, allocation with a connection-bound Ed25519
proof, attach-on-allocate, owner-UID `list`, and `renew`. Term names use the
canonical UID encoding and 110 random bits; the epoch exclusion set is bounded
to 65,536 names. Exhaustion refuses allocation. Each UID may hold 64 nonterminal
Terms. Retained mutations use increasing decimal IDs and bounded cached results.
Clients renew every five seconds; successful renewal refreshes the 15-second
CLOCK_BOOTTIME lease, including repeated request IDs. Disconnect or expiry
suspends the record for 30 seconds. Delayed maintenance uses the original lease
deadline and processes expiries in time order, preserving earlier descendant
windows. The other S2 boundaries provide child grants, resumption, notices and
the typed client API.

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
[BROKER-016–025](../../spec/05-broker-topics.md). S2 builds the binding lifecycle
on that foundation. The strict parser still rejects malformed requests before
mutation. Recipient verb/property policy enforcement remains S4 work.

## S2 first boundary: reserve the allocation namespace

The BROKER-017 shape `^[tc][0-9a-z]{1,7}-[a-z2-7]{22}$` is now refused by
`noded.register` on every ingress, including brokers without a Unix listener.
Refusal returns `rc:10` with `reserved_name` and preserves any previous
registration on that connection. Leading-zero and overflowing UID lookalikes
are reserved too; the check does not depend on allocation state or interpret
the displayed UID as authority. Neighbouring legacy names remain valid.

The p0i-02 namespace slice exercises TCP and real Unix WebSocket connections,
pre-claim refusal, retained alias authority and forged `from` canonicalisation.
The broker starts with an empty, non-persistent registry;
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

`VerifiedConnection` now exposes `session_hello`, `session_allocate`,
`session_grant_create`, `session_grant_fetch`, `session_challenge`, `session_prove`,
`session_renew`, `session_revoke`, `session_list` and `session_lease_check`.
An empty connect name selects anonymous Unix bootstrap. Calls are serialised
per verified handle; responses require explicit native framing and RC. Allocation
signs the exact BUS-016 bytes with Ed25519. Challenge signing requires independently
retained expected UID, parent-key hash, pane, role, key hash and capability hash;
the application also tracks pane-generation high-water within each parent instance.
No uncertain mutation is retried automatically. Wake errors remain visible on
success and refusal. Private signing keys are never serialised into requests.

Same-UID discovery includes the broker-owned `native_session` snapshot for attached,
pending and suspended records. Other
transports and UIDs see bare allocated names. Caller provenance cannot set the
field. Session list and discovery share `SessionRecord`; neither is live authority.

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
authorisation requires correlated lease checks. Use `client().close()` for explicit
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
