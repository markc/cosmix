---
title: Broker routing, discovery and topics
chapter: 5
version: 0.2.2
status: draft
date: 2026-09-10
---

# Broker routing, discovery and topics

## Routing and registered identity

This chapter distinguishes requirements, the source profile at `96d12fdf`,
and unresolved differences. No live deployment or test-run claim is made.

**BROKER-001:** The node broker routes local service names and in-mesh
addresses over registered connections and mesh peers. Cross-mesh `@FQDN`
targets remain parser-reserved and router-refused. Request IDs may be replaced
internally to avoid caller collisions; replies must be returned to the
correct originating connection with its correlation restored. Delivery to a
socket queue is not acknowledgement of application execution.

**BROKER-002:** `noded.register` takes the requested service name from `from`
and optional `RegisterProvenance` JSON from the body, not a `body:` header:

```text
---
command: noded.register
from: exampled
id: 0192b3a4-5e6f-7890-abcd-ef1234567890
---
{"version":"0.1.0","binary":"cosmix-exampled","pid":4242}
```

The checked broker validates `^[a-z][a-z0-9-]{1,30}$`, binds the name to the
connection and refuses an occupied live name. It stamps `registered_at`.
Malformed provenance currently produces a warning and name-only registration,
not total rejection. `noded.deregister` operates on the calling connection's
name and is idempotent. Disconnect cleanup must not remove a newer connection
that acquired the same name.

**BROKER-003:** Forwarded `from` MUST be derived from the registered connection;
anonymous caller assertions must not impersonate a service. Anonymous topic
participants receive broker-local connection identities; these are neither
persistent principals nor federation credentials. Broker admission and signed
mesh identity have their own contract; a registered name alone does not prove
an external principal.

**BROKER-004:** The recipient broker MUST remove every case-insensitive spelling
of caller-supplied `broker_origin`, then stamp `local` only for its same-node
socket classifier and `mesh` otherwise. This applies to delivered requests,
events and correlated responses. A topic retains publish-time origin, not
subscriber-time origin. An authorisation consumer MUST require exactly one
`local` value where local delivery is required; missing/unknown/mesh fails
closed. Direct broker notifications are outside this authorisation contract.

## Discovery

**BROKER-005:** `noded.list` returns service records; `noded.info` returns live
node information; `noded.peers` describes peer configuration; `noded.ping`
reports liveness and extension versions. The checked ping advertises
`core: "1.0"`, `topic: "1.0"`, `observe: "1.0"`. An absent extension is not
assumed available. `core` is not a claim of renderer conformance. Clients
MUST tolerate unknown fields and read record fields by key.

**Intended addition:** when available, the same extension map adds
`"native-session": "1"` (BUS-013). TCP callers may discover availability but
MUST use Unix ingress to participate; advertisement is not authentication.

`ServiceInfo` has required `name`, optional binary/version/git SHA/dirty/build
time/PID/start time/registration time/schema version and `meta`. It is stored
identity/provenance, not a live health snapshot. Its deserialiser also accepts
legacy bare-name strings. `NodeInfo` computes uptime and service count on read.
Source: [discovery records](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/crates/cosmix-lib-bus/src/service_info.rs).

## Topic operations

**BROKER-006:** A topic is a named latest-value channel, not durable history.
State is in memory and disappears on broker restart. The broker parses the
inner envelope for routing annotations and can inspect namespace metadata for
property filters; it does not own application state or promise byte identity.

| Command | Input | Successful response / effect |
|---|---|---|
| `topic.publish` | `name` header, inner ABP envelope in body; `retain` defaults true | `{seq, delivered}`; count is successful queue insertions |
| `topic.subscribe` | `name` header | `{subscription_id, replayed, seq}`; replay can precede acknowledgement |
| `topic.unsubscribe` | `name` header | `{}`; removes the peer's filter variants too |
| `topic.subscriber_count` | `name` header | `{count}`; absent topic gives zero |
| `topic.list` | optional `prefix` header | metadata array; prefix is literal, not glob |
| `topic.clear` | `name`, optional `notify` default true | clears snapshot; optional delete delivery |

**BROKER-007:** Publish bodies MUST be complete single-message ABP envelopes.
The checked broker rejects input above 1,048,576 bytes with RC 10
`payload_too_large` and `limit: 1048576`; invalid framing gets
`malformed_payload`. It currently uses lenient compatibility parsing, so this
error does not prove full header/schema validation. `$`-prefixed publication
is rejected as `reserved_name` by the checked implementation. Inputs MUST use
canonical boolean `true`/`false`; the implementation treats only exact `false`
as false for `retain` and `notify`.

```text
---
command: topic.publish
to: noded
name: example.state
retain: true
---
---
command: example.state.changed
type: event
---
{"ready":true}
```

**BROKER-008:** For accepted publications the broker allocates the topic's
sequence and replaces producer `topic`, `topic_seq`, `topic_stale`, `topic_op`
annotations with broker-owned values. Live subscribers receive the annotated
inner command. Retention caches that envelope; `retain: false` does not replace
an existing snapshot, but increments the sequence and can clear staleness for
the same publisher. Sequence is monotonic within one topic-state lifetime,
not across deletion/purge/recreation or broker restart. No global ordering,
exactly-once delivery, persistence or multi-producer exclusion is promised.

**BROKER-009:** Ordinary subscription is idempotent per connection/topic.
An existing identical subscription is not replayed twice. Property watch
grants can add namespace-filtered variants; the underlying identity is then
peer/topic/filter, and counts can count variants rather than distinct peers.
Subscribers MUST accept gaps and resynchronise from the owning service where
correctness depends on complete history. Queued deliveries can outlive an
unsubscribe acknowledgement; topic unsubscribe has no observation-style purge
fence. Concurrent publisher ordering/replay atomicity requires dedicated
testing before any stronger claim.

**BROKER-010:** Publisher disconnect marks its retained snapshot stale; replay
adds `topic_stale: true`. The source uses a 60-second grace and 10-second
janitor interval. Re-publication refreshes state. No-snapshot/no-subscriber
entries are removed, so sequence can restart. `topic.clear` with notification
sends `topic_op: delete`; consumers decide how invalidation affects their own
state. The broker does not remove application windows.

`topic.active` and `topic.idle` notify the last publisher of zero/non-zero
subscriber transitions. They are best-effort advisory notifications, including
for anonymous connections; racing publishers do not each receive a notice.

**BROKER-011:** Reserved property topics enforce owning-service publication
and restricted subscription/grant paths. Direct subscription to records/audit
topics is rejected; callers use owner `props.watch`/`props.audit.watch` for
authorisation and namespace filtering. This disproves the old blanket claim
that all topic ACLs are future work. The property chapter owns the detailed
grant and canonicalisation contract.

## Backpressure and delivery limits

**BROKER-012:** Routing MUST remain bounded under slow consumers. The current
per-peer outbound channel holds 256 messages; `try_send` on full drops the
new message and retains the connection. Closed channels are pruned. The old
drop-oldest plus slow-consumer-disconnect design is **superseded for this
profile** by the accepted [compatibility amendment](compatibility-profile.md).
Clients must allow notification loss without a disconnect. A message-count bound is not a small memory
bound: payload limits and aggregate topic/subscription counts also matter.

Neither publish success nor `replayed: true` is a receipt from the subscriber
application. Brokers and clients need queue-pressure tests, including replay
under a full outbound queue and concurrent publish/re-subscribe, before
claiming stronger semantics. Retained topics are unsuitable as the sole audit
history or guaranteed workflow-delivery mechanism.

## Observation

**BROKER-013:** `noded.observe.start/stop` is a separate bounded observation
extension over canonicalised accepted envelopes and route outcomes. Control
and observation-event frames are excluded to prevent recursion. Start requires
a registered, same-node, configured allowlisted service; the default allowlist
is empty. Subscription ownership is connection-scoped. `noded.tap` remains a
deprecated, unredacted legacy surface, not the recommended substitute.

Start body carries optional `filter` (`verbs`, `services`, `directions`),
`body: "none"|"redacted"`, and capacity. Defaults/bounds: 1024 events,
64–4096 allowed, 8 MiB per subscription, 4 subscriptions per connection,
16 per broker, 32 verb globs of at most 128 bytes and 64 service filters
of at most 128 bytes each.
Payloads above 64 KiB become metadata only. Unlike ordinary routing, the
observation ring evicts oldest; a 2 ms drainer has a 1 MiB per-subscription
wire budget per wake. Routing never waits for an observer.

**BROKER-014:** Observation redaction MUST run before enqueue: first omit
policy-sensitive verbs (registration/admission/auth/login/token families),
then recursively redact credential-like keys from structured data; opaque
payloads are omitted. Events carry subscription ID, sequence, time, direction,
outcome, message type, canonical endpoints/verb, byte size, correlation ID,
RC and drop count, plus permitted payload and omission reason. Start ACK
precedes events; stop purges pending events before ACK and forms a fence.
Non-owned/absent stop IDs return `{stopped:false}` without an existence oracle.
Errors are RC 10 `observe_unauthorised`, `observe_invalid_args`,
`observe_filter_invalid`, `observe_limit_exceeded`, or RC 20
`observe_unavailable`.

Source: [observation](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/crates/cosmix-noded/src/observe.rs).

## Specification distribution and migration

**BROKER-015:** A served specification MUST identify the suite revision and
contract it represents. Reading-order chapter numbers MUST NOT silently reuse
legacy `spec.get chapter=N` or `world.specs.NN` identities for different content.
Migration requires an explicit old-ID registry and compatibility policy.

At the baseline, `spec.get` accepts `args.chapter` or exact `args.name` and
returns document headers/body; errors use RC 10 with available chapters where
appropriate. Discovery accepts only `NN_*.md` numeric prefixes, not this
candidate's hyphen names or the old dated filenames. Explicit `--spec-dir`
and `COSMIX_SPEC_DIR` select a directory, followed by legacy discovery.
Exact-name lookup rejects path separators and parent traversal. This does
not establish symlink confinement for an untrusted directory.

`world.specs.NN` is seeded at startup. Automatic edit-triggered republish is
not established. Merely placing files under `docs/spec/` does not switch the
broker's discovery or topics. Candidate exact-name access is possible only
with an explicitly configured directory; production cutover remains gated on
the registry/discovery migration and end-to-end checks. Optional service
`SPEC` and a wildcard `world.*` subscription are not universal implemented
bootstrap assumptions. Topic subscriptions are exact names.

## Native session identity (intended)

BROKER-016–025 are proposed implementation contracts. The mechanism,
default-open UID trust boundary and Term-owned lifetime are accepted design
decisions; none is a claim of implemented enforcement at the checked baseline.
Wire validation and proof bytes are specified in
[BUS-013–017](04-bus-wire.md#native-session-wire-profile-intended).

**BROKER-016 — Additive assurance (intended).** BROKER-002/003 remain the legacy
name-to-connection contract. Existing citizens and D2 mesh peers MUST continue
to work unchanged; legacy registration MUST NOT establish verified Unix-user,
Term or pane identity. The new profile uses one system broker per node with
additional Unix ingress, not per-user brokers or a parallel control protocol.
Protected controls MUST require the new profile; unavailable bootstrap MAY
leave graphics usable but MUST NOT enable a global unauthenticated `term`
control fallback. Remote D2 origin proof is not local UID proof; remote session
delegation and verified login-session roots are outside version 1.

**BROKER-017 — Allocated names (intended).** Noded MUST allocate Term routes as
`t<uid-base36>-<22-base32>` and child routes as `c<uid-base36>-<22-base32>`.
The UID is kernel-derived u32 encoded in lowercase base36 without leading zeros
(zero is `0`); the suffix consists of 22 independent, uniformly random symbols
from `abcdefghijklmnopqrstuvwxyz234567` (110 bits). Names remain at most 31
ASCII bytes. The displayed UID MUST NOT be used as proof of ownership.

The entire shape `^[tc][0-9a-z]{1,7}-[a-z2-7]{22}$` is reserved on every
registration ingress, including leading-zero and overflowing-UID lookalikes,
and while a name is unallocated, expired or revoked. `noded.register` MUST
refuse that shape; allocation MUST issue only the canonical encodings above.
Activation MUST check for pre-existing legacy collisions with this grammar and
refuse profile activation rather than evict a citizen. Allocation MUST atomically
check the registry, reservations and names already issued in the current broker
epoch, then insert the ownership record and reservation. On collision, retry up
to eight times after the initial candidate (nine candidates maximum), then return
`RESOURCE_LIMIT`. Issued names MUST NOT be reused within an epoch; implementations
MUST bound this set and refuse allocation on exhaustion. No silent replacement
or client-chosen privileged name is permitted. Panes are generation-qualified
objects of a Term instance, not service aliases. Version 1 has one primary
connection per record; auxiliary attachment roles are deferred. Allocated names
MUST NOT be used as suffix bases (`-sub`/`-observe` can exceed 31 bytes).

**BROKER-018 — Session command API (intended).** All commands below target
`noded` over authenticated Unix ingress. The prefix is `noded.session.`.
Every listed input field is required; `?` denotes an optional field and its
default is stated below. No other arguments are accepted. IDs, keys, counters
and deadlines use BUS-016 encodings. A `ref` is exactly
`{record_id,incarnation,binding_generation}`. Record owner, parent and UID are
derived from authenticated state, never caller assertions.

| Suffix | JSON input | RC 0 result and effect |
|---|---|---|
| `hello` | `{}` | `{broker_epoch,connection_id}`; convenience read of this connection's public proof context, not an allocation gate |
| `allocate` | `{public_key,signature,policy?}` | `{record}`; verify BUS-016 allocation proof, allocate and attach a Term record to this unbound connection; policy defaults to `default-open` |
| `grant.create` | `{parent:ref,pane_id,pane_generation,public_key,role,capabilities}` | `{grant,record}`; attached parent Term reserves its child's route and pending grant |
| `grant.fetch` | `{public_key}` | `{grant,record}`; issuing attached Term fetches its grant by exact public key |
| `challenge` | `{record_id,incarnation,purpose,grant_id}` OR `{public_key,purpose:"enrol"}` | BUS-016 challenge object; returned purpose determines proof encoding; optional `wake_error` reports degraded wake registration |
| `prove` | `{challenge_id,signature}` | `{record}`; atomically enrol or resume on this connection |
| `renew` | `{target:ref}` | `{record}`; renew this connection's current attachment |
| `revoke` | `{target:ref}` | `{revoked:true}` or `{revoked:false}`; revoke own record or an owned child and its pending grant |
| `list` | `{}` | `{broker_epoch,records}`; bounded owner-UID snapshot including pending and suspended records |
| `self` | `{record_id}` | `{record}`; one owner-UID record, including terminal state; absent and foreign-UID IDs both return `FORBIDDEN` |
| `lease.check` | `{target:ref}` | `{lease_remaining_ms}`; authorised recipient obtains a fresh remaining-lease delta and lifecycle interest |

`allocate` requires an unbound kernel-verified local owner connection; its key
is registered for later resumption. Allocation cannot require a pre-existing
session grant; at most 64 nonterminal Term records per verified UID are allowed,
including suspended records. Excess returns `RESOURCE_LIMIT`. An existing child
attachment cannot allocate a parent role on that bound connection. `policy`
accepts only `default-open` or `restricted`; `role` in
`grant.create` is exactly `pane-shell`. The caller MUST be the attached parent
Term; grant creation declares the pane ID/generation within that parent's
instance and records its ownership atomically. Only one nonterminal child record
per parent/pane/generation is allowed. Capabilities MUST contain one to six unique
BROKER-023 tokens; the parent MUST NOT delegate authority beyond that pane.
Noded MUST retain a generation high-water mark per `(parent_instance,pane_id)`;
each new grant requires a strictly higher generation than any previous grant
for that pane, otherwise `STALE_GENERATION`. Re-granting after expiry therefore
advances `pane_generation` even when the same live child/key is
retained; it cannot revive the old grant or authorise old target generations.
High-water marks survive grant/child-record pruning for the parent's lifetime.
A key may identify at most one nonterminal session record per UID;
ambiguous key reuse returns `CONFLICT`, reason `key_in_use`.

Allocated Term records hold all six capabilities and their selected policy;
children hold the delegated subset and inherit their parent's policy.
Each record has a random `record_id`, `instance_id` and `incarnation`. For a
Term, `instance_id` identifies that Term; for a child it identifies the child.
Children additionally hold `parent_instance` and `parent_incarnation`. A Term
has null parent/pane fields. IDs MUST NOT be derived from PID, service name,
title, environment or a client-supplied UUID. A valid public key MUST be checked
before allocation. Parent allocation establishes UID ownership, not executable
attestation or child-grant authority over another Term.

`challenge` MUST check the requesting connection's verified UID equals the
record owner before returning any scope or state. Unknown and unowned selectors
return BUS-017's identical `FORBIDDEN`. A grant selector must also belong to
that record and UID before any grant-state detail is returned. Both selector forms are exact objects,
not combinable. The record form accepts `purpose:"enrol"` with a grant ID or
`purpose:"resume"` with present-null grant ID. The key form searches only within
the requesting UID: a pending grant yields enrol; an attached/suspended record
with a consumed grant yields resume, with null grant fields. A live Term key
also yields resume. Terminal/absent records return uniform `FORBIDDEN`.
Record-form enrol against a consumed grant returns `CONFLICT` with
`details:{"reason":"grant_consumed"}`; resume against pending returns
`CONFLICT` with `details:{"reason":"binding_pending"}`. These state details
are returned only after UID ownership is established. `grant.fetch` additionally
requires the current issuing parent attachment; absent/unowned keys return the
same uniform `FORBIDDEN`. It does not change state or extend expiry.

A key-selected challenge also attempts to register at most one bounded
`(verified UID,public_key)` interest per connection, atomically with lookup,
including when lookup returns uniform `FORBIDDEN`. Changing that connection's
key requires closing it; the per-UID interest cap is 256. Interest-quota refusal
MUST NOT block challenge lookup or replace its result. If registration fails,
the response additionally carries an unsigned `wake_error` object:
`{"error_code":"RESOURCE_LIMIT","message":"wake registration unavailable","details":{"reason":"interest_limit","retry_after_ms":"60000"}}`.
The lookup's rc/body is otherwise unchanged, including identical absent/unowned
errors. The extra error depends only on the requester's quota, not resource
existence. Existing interests do not consume another slot on repeated lookup.

This is an explicit degraded recovery path under active same-UID denial of
service, including restricted policy: the client MUST report unavailable wake
registration and MUST NOT silently wait indefinitely. A lookup after grant
creation needs no interest and can complete immediately. For child-first
ordering, the client MAY repeat once after `retry_after_ms`; if neither a
challenge nor a wake can then be obtained, it MUST surface the recovery failure
rather than enter an unbounded retry/poll loop. No restricted authority is gained.

Interests reveal no other UID's records and grant no authority. Subsequent
pending-grant creation MUST atomically offer BROKER-022's lifecycle notice to
matching interested connections. A parent transition to attached MUST likewise
atomically offer notices for its nonterminal children to connections interested
in their UID/key pairs, even when no grant is created. A resume proof refused
because the parent is suspended MUST retain any registered interest; only its
one-use challenge slot is consumed. The child re-requests a key-selected
challenge on notice or gap. Disconnect removes the interest.

Retained mutations are exactly `allocate`, `grant.create` and `revoke`. They
MUST use positive canonical u64 decimal request IDs, strictly increasing per
connection for each new accepted mutation. Results are retained for 15 minutes
or the last 1,024 accepted mutations per connection, whichever expires first,
within a broker-instance cap of 8,192 results and 16 MiB. Earlier eviction is
allowed at that cap. Retained identical command/typed-argument retries return
the original result; changed arguments/command return `CONFLICT`, reason
`request_mismatch`. Each connection MUST retain its accepted-ID high-water mark
until close: an ID at/below it without an eligible cached result returns
`CONFLICT`, reason `unknown_outcome`, never re-executes. Pruning an affected
record invalidates its cached result with the same outcome. Out-of-order new
IDs below that mark are refused too. Exhausted u64 space requires reconnect.
After connection/epoch loss, clients MUST reconcile with `grant.fetch` or
key-selected challenge and MUST NOT automatically resubmit an uncertain
allocation as a fresh mutation. No cross-restart dedupe is claimed.

`hello`, `renew`, `list`, `self`, `grant.fetch` and `lease.check` are exempt from
result retention. Every successful renew, including a repeated ID, MUST refresh
the current live lease deadline; it MUST NOT revive a suspended/terminal record.
Challenges/proofs use their one-use state instead: any repeated consumed proof
returns `CONFLICT`, reason `challenge_consumed`; pruned/unknown challenge IDs
return uniform `FORBIDDEN`. `list` MUST return `RESOURCE_LIMIT` rather than a
silently partial snapshot. An already revoked owned target returns
`{revoked:false}`; stale references return `STALE_GENERATION` and MUST NOT affect
a successor. Missing/unowned targets return uniform `FORBIDDEN`.

For lost proof ACK recovery, the child requests `{public_key,purpose:"enrol"}`
again. Pending produces an enrol challenge; committed attached/suspended
produces a resume challenge. The child follows the returned purpose and signs
fresh bytes; resume commitment replaces the old attachment. This works under
both target policies without ambient list access. After broker restart, Term
creates a fresh grant with the retained child's same public key, and the child
uses this same lookup; the new grant ID comes from the broker, not the consumed
memfd. If grant creation's ACK was lost, Term MUST fetch by key before issuing
another grant. Missing bootstrap remains control-free under restricted policy.

**BROKER-019 — Launch grants and proof (intended).** Term MUST generate a fresh
Ed25519 keypair for the initial child record, retained across that child's
re-grants and replacement records within and across broker epochs. It submits
only the public key to `grant.create`, and MUST
deliver the private key plus public descriptor through an inherited sealed
anonymous memfd. A nonsecret environment marker MAY identify the FD number.
The child MUST consume it before startup hooks/user source, close the FD and
remove the marker; later children MUST inherit neither FD nor key material.
For its entire process lifetime the private key MUST remain inaccessible from
the Mix language surface: no builtin, variable, property or introspection path
may expose it. Retaining it for fresh challenge signing is not language access.
The spawning parent MUST close and clear its private copy. PTY stdio is not the
bootstrap channel. Private keys MUST never appear in ABP, argv, environment
values, properties, tap, observe or logs. Sealing provides integrity, not secrecy.

A grant descriptor is
`{grant_id,record_id,incarnation,public_key,parent_key_hash,expires_ms,state}`;
the accompanying record supplies immutable owner, parent, pane and role scope.
The parent-key hash is broker-derived SHA-256 of the parent's allocation-proven
public key. The child MUST retain it with its private key and initial scope so
restart recovery cannot silently bind it to another Term that knows its public
key. Public-key lookup is not permission to change the expected parent.
The broker MUST retain four grant states: `pending`, `consumed`, `expired`,
`revoked`. Only pending grants may enrol. They expire 30 seconds after minting;
pending→consumed occurs atomically with successful attachment, pending→expired
at expiry, and pending→revoked on cancellation/owner revocation. Expiring or
revoking a pending grant MUST revoke its pending child record and remove its
route reservation, while retaining the issued-name exclusion. Terminal states
never become pending again. Revoking an enrolled child revokes its attachment;
its grant remains consumed. Terminal history MAY be pruned under bounded
retention, but absence MUST NOT permit proof replay or name reuse.

There MUST initially be at most 32 pending grants per parent Term record and
1,024 globally. The per-parent figure is a tunable advertised limit; the global
cap and 64 live-Terms-per-UID allocation cap also apply. One Term MUST NOT spend
another Term's per-parent quota. Configuration may lower these caps.
Challenges expire after five seconds, no later than their pending grant, with
at most one outstanding per connection and 128 per verified UID. Repeating the
identical selector on that connection returns its existing unconsumed challenge
without extending the deadline. A different selector returns `CONFLICT`, reason
`challenge_outstanding`, with `retry_after_ms` until expiry. After consumption,
expiry or disconnect a fresh challenge is required. Quota exhaustion returns
`RESOURCE_LIMIT`; expired grants/challenges MUST be excluded from quota counts.
Every mutation MUST check current deadlines; a janitor alone is insufficient.

Enrol requires matching verified UID, pending grant, scope, epoch, incarnation,
key and fresh connection-bound signature. Wrong pane, role, principal or proof
MUST NOT consume the legitimate grant. Any identifiable prove attempt MUST
atomically consume this connection's outstanding challenge and release its
slot, whether arguments/signature succeed, fail, or are malformed; it MUST NOT
consume another connection's challenge. The failure path cannot extend its deadline.
Successful verification, grant consumption and route installation MUST commit
atomically against revocation. Retrying requires a fresh challenge. Pane-scoped
binding authority does not exist before attachment; under default-open an
independent same-UID connection still has ambient owner authority (BROKER-023).
Captured proofs, including successful ones, MUST fail on another connection,
another challenge, after revocation and after a broker restart.

On pending-grant expiry/revocation the broker MUST offer BROKER-022's lifecycle
notice to the attached issuing Term. For a still-live, never-enrolled child,
Term SHOULD respond by creating a fresh grant with the same key and pane ID,
advancing pane generation under BROKER-018, subject to quotas. Retry is driven
by the notice or gap-triggered reconciliation, not grant-state polling. The child receives the new generation
in its key-selected challenge and MUST reject older-generation requests.

**BROKER-020 — Attachment and resumption (intended).** Each broker start MUST
mint a fresh random epoch. Attachments have states `attached`, `suspended`,
`revoked`; grant-reserved child records start `pending`. Binding generation is
`0` for a pending record, `1` on initial attachment, and increments on each
successful resumption. Connection loss MUST suspend routing/control on detection;
it MUST NOT erase the logical record or enable name-based registration. Parent
suspension MUST also suspend descendant binding authority, close their old
attachments and start their resumption windows. Suspension is idempotent:
already-suspended descendants retain their original deadlines, never extend
them on another ancestor suspension. Resuming a parent does not
implicitly authenticate child connections; each child must re-prove, and child
proof requires its parent to be attached and live. Suspension MUST retain the
binding key/record's child scope. It does not remove independent ambient owner
authority under default-open policy.

An attached client MUST renew every five seconds; its lease expires 15 seconds
after attachment or the last successful renewal. Explicit disconnect or lease
expiry suspends the record and starts a 30-second resumption window. Suspended
records grant no control rights. Renew does not resume; a fresh challenge and
proof against the stored public key are required. A resume challenge proposes
the next binding generation, has null grant fields, and expires no later than
the resumption window when suspended. An attached record may also resume by
fresh key proof (lost-ACK recovery or connection replacement); its challenge
expires no later than the current attachment lease. Challenge issuance MUST NOT
disconnect the current owner. Commitment MUST compare current generation,
verified UID, stored key and parent liveness, then atomically close/replace the
old attachment and increment generation. If the proving connection is already
that record's attachment (lost ACK), it stays open while generation advances;
otherwise the old channel is closed. An attachment to a different record on
the proving connection MUST be refused, not silently renamed or widened.
A competing stale proof cannot replace
the successor. Key-selected lookup never changes the record's pane/capability scope.
An expired window recursively revokes the record under BROKER-022. Expired or
revoked records cannot renew; child renew MUST fail if its parent is terminal
or not attached, irrespective of the child's own remaining lease.

The same binding key MAY prove fresh resume challenges; the launch grant itself
is never reused. A new connection MUST prove again even if its UID/PID or service
name matches. Clients MUST not expose buffered commands as authorised before
resumption. A surviving Term after broker restart or same-epoch lease/window
expiry allocates a fresh incarnation
using its retained parent key, and re-enrols its still-owned children with fresh
grants/challenges and their retained public keys. Old epochs, requests and subscriptions are not restored
as authority; a child that lost its key cannot reclaim an old identity by name.
Parent-key possession alone anchors parent continuity; instance/incarnation
identify the new scope, not an ordering or continuity test. A Term-PROCESS
restart is unrecoverable in place: its parent key dies with the process and
version 1 has no secret store. A restarted Term MUST restart its children with
fresh keys; only a surviving Term can recover them across a broker restart or
broker-record expiry.

Lease stamps carry remaining-millisecond deltas, not recipient-comparable broker
timestamps. Before relying on a bound caller lease or installing a private
watch, a recipient MUST have an unexpired cached `lease.check` for that reference,
or complete a fresh check outside authorisation resolution first. Resolution
MUST remain synchronous and MUST NOT block on a Bus call. Fresh checks run
outside resolution per lease window or after a lifecycle gap; without an eligible
cache, resolution grants no authority. A fresh check records local `CLOCK_BOOTTIME`
at request start `s`. The broker responds with its current nonnegative remaining
milliseconds `r` (minimum across the record and every ancestor lease) only if
target reference and parent are live and the requesting
connection is an affected recipient (BROKER-022). The recipient's conservative
deadline is `s+r`, never receive-time plus `r`; if already elapsed it MUST refuse.
This needs no shared time-namespace offset and cannot extend authority through
transit delay. The check also registers lifecycle interest atomically. Zero
remaining time/terminal targets return `EXPIRED`; stale references return
`STALE_GENERATION`. For a verified same-UID recipient and an existing target,
an absent/aged-out dependency returns `CONFLICT` with
`details:{"reason":"dependency_missing"}`; this does not assert retained
history or re-establish delivery. A fresh authorised bound delivery must register
the dependency before the check can succeed. Absent/unowned targets and other
unrelated callers retain uniform `FORBIDDEN`.
Checks MAY be cached per epoch/reference up to that deadline. Only a fresh
non-replayed stamped request plus a new correlated check may extend recipient authority;
broker renew alone and replayed snapshots MUST NOT extend a cached watch lease.

**BROKER-021 — Discovery assurance (intended).** Live service discovery MUST add
an optional broker-owned `native_session` field, never trust or overwrite it
from `RegisterProvenance.meta`. `session.list` uses the same record shape:
`name`, `record_assurance`, `owner_node`, `owner_uid`, `broker_epoch`, `record_id`,
`instance_id`, `incarnation`, `role`, `parent_instance`, `parent_incarnation`,
`pane_id`, `pane_generation`, `binding_generation`, `state`, `capabilities`,
`policy`, `lease_remaining_ms`. `name` follows BROKER-017; owner UID is a u32
JSON integer; `role` is `term` or `pane-shell`; policy is BROKER-023's token.
IDs/counters use BUS-016. `lease_remaining_ms` is present-null when unattached;
otherwise it is recomputed at delivery, never an absolute recipient deadline.
The state is BROKER-020's state; never-enrolled records have `record_assurance`
`reserved`; records that completed attachment have `session-bound`. This is historical assurance,
not permission while suspended/revoked. Parent/pane fields are null for Terms;
all are present and non-null for children. Capabilities are sorted unique tokens.
Record `record_assurance` and BUS-014 connection `assurance` have distinct value
spaces. Remaining-time discovery fields are informative; BROKER-020 governs
recipient lease checks across time namespaces.

`session.list` is restricted to the verified owner's UID. General `noded.list`
MUST expose `native_session` descriptors only to kernel-verified callers of the
record's owner UID. Other callers see the bare allocated name only, without
record/provenance fields that disclose the identity graph. Neither view may
expose private contents or credentials. Legacy records have no `native_session` field.
Clients MUST distinguish absent assurance from verified identity and tolerate
unknown discovery fields. Diagnostic provenance PID is not a process identity.

**BROKER-022 — Revocation and races (intended).** Version 1 lifetime is owned
by Term. Pane close MUST revoke local authority before child cleanup; child exit
MUST revoke that child's authority and grants. Term exit MUST recursively revoke
its records, bounded by connection detection and lease/resumption deadlines when
notification is lost. Broker restart invalidates all prior epoch authority.
Logout is not an independent revocation root: a lingering Term retains only its
own panes' authority. Verified login-session roots are deferred.

Every parent transition to `revoked`, including explicit revoke and resumption
window expiry, MUST recursively revoke all descendant attachments and pending
grants and remove their routes/reservations. This transition MUST be atomically
ordered against descendant prove/renew; neither may commit after ancestor
revocation. Issued-name exclusions and generation high-water marks remain.

Removal, replacement and stale cleanup MUST compare record ID, incarnation,
binding generation and channel ownership together; a delayed disconnect MUST
NOT remove a successor attachment. Parent-initiated child revoke MUST verify
the parent's current ownership and the child's current reference, then remove
only that captured child's channel. Receivers MUST recheck live pane state,
scope and generation under their mutation lock. Revocation MUST invalidate
input leases, pending grants and private watches and discard uncommitted input.
Previously written PTY bytes or started operations cannot be recalled; responses
MUST report partial or unknown outcomes rather than promise rollback.
Best-effort revoke events alone are insufficient: recipients MUST enforce the
conservative lease-check deadline and invalidate cached authority on broker
epoch/connection loss.

The broker MUST offer `noded.session.lifecycle`, `type:event`, with body exactly
`{target:ref,state,broker_epoch}` on the ordered connection stream. Header IDs
are diagnostic only; no acknowledgement or correctness decision depends on them.
It contains metadata only. State is `pending`, `attached`, `suspended` or
`revoked`; grant expiry/cancellation yields its pending record's `revoked`
notice. Only the broker may emit this command: it MUST refuse client-authored
copies at every ingress, including correlated replies. Inner-envelope refusal
MUST occur at BROKER-006's inner-envelope parse before topic publication or
retention, using BROKER-007's `reserved_name` refusal pattern. This also applies
to the broker-only gap command below.
Recipients MUST accept notices only through their authenticated broker channel.
Send each transition to the record's current/closing connection, its
attached parent, and affected recipient connections. Before delivering a bound
request to a service, the broker MUST record that recipient connection as an
affected recipient until the stamped lease expires. `lease.check` and private
watch installation MUST register/refresh that dependency atomically with their
success. This is a connection/reference dependency, not a service-name lookup.
The broker MUST advertise dependency caps of 256 distinct references per
recipient connection and 8,192 globally. Existing-reference refresh uses no new
slot. At either bound it MUST refuse the bound delivery itself with
`RESOURCE_LIMIT`, reason `recipient_dependency_limit`, before the recipient
receives the request; it MUST NOT silently skip dependency registration.
Watch installation or refresh requiring a new slot is refused on the same terms.
Expired dependencies and connection loss release slots.

Notices are best-effort accelerators; BROKER-012's notification loss without
slow-consumer disconnect is RETAINED for this profile. There MUST be no notice
ACK, retry timer or retry-driven polling. Recipients MUST apply notices
idempotently and ignore stale generations for successor attachments. A newer
generation invalidates cached lower-generation authority for that record, even
if state is attached. Notice queues hold at most 256 entries per connection
and 4,096 globally. Per-connection overflow drops the incoming notice and sets
that connection's sticky lifecycle-gap bit. Global exhaustion MUST shed an
entry from a largest-backlog connection's queue and set that connection's bit;
it MUST NOT close any connection. Ties may choose any largest backlog.

The gap indicator is one coalesced bit per connection, separate from bounded
queues. At the next writable moment the broker MUST send the broker-only event
`noded.session.lifecycle.gap` with body exactly `{broker_epoch}`, ahead of later
queued deliveries. It consumes the bit for that write attempt and restores it
if the write cannot complete; concurrent new overflow MUST leave the bit set
for a subsequent writable moment. No queued gap message, ACK or timer is needed.
On a gap, recipients MUST invalidate cached lifecycle state and resynchronise
before trusting it: services re-issue `lease.check` outside resolution, children
re-request their key-selected challenge, and parents reconcile pending grants
with `grant.fetch`/`session.list`. Failed resynchronisation grants no authority.
Notices and gaps only accelerate reconciliation; neither is a delivery guarantee.
A lease-authorised watch MUST expire no later than its recorded local deadline
unless refreshed by a fresh stamped request and correlated check; a missed
notice can never extend that deadline. Ambient owners have no lease to renew.

**BROKER-023 — Policy and threat boundary (intended).** `default-open` is the
accepted default: an independent kernel-verified same-UID local owner process
receives `read_state`, `read_contents`, `input`, `execute`, `manage_layout` and
`terminate` for that user's Term instances without human confirmation. A bound
`pane-shell` identity receives only its granted capabilities for its pane; it
MUST NOT widen that binding's scope by changing name or resuming its key/record.
Services MUST apply scope to the bound connection. Scope preservation rests on
the stored binding key and record generation, never PID or process-lifetime
inference. A resume proof MUST NOT change the record's role or capability scope.
Independent ambient connections are outside that binding: under default-open
the same UID, including a child on another connection, has owner authority.
Pre-enrolment and sibling-scope denial are therefore asserted under restricted
policy only; cross-UID denial applies under both policies. This is not a same-UID
process sandbox or executable attestation.

Policy is an attribute of the TARGET Term record, enforced by the recipient
from its own record; it is not inferred from a caller stamp or caller-selected
policy. The opt-in `restricted` policy removes ambient owner access to protected
operations: only the owning Term and explicitly granted, current principals
may act. Grant issuance is machine-checked and unattended. The initial grant
role is pane-shell; independent restricted automation requires an explicit
delegation role in a later extension, not a guessed alias or a bypass.
Other UIDs, unverified TCP/anonymous callers and undelegated mesh principals
MUST be denied protected operations in both policies. Here anonymous means
without a verified principal, not merely without a registered service name.

Services MUST enforce these same capabilities on verbs and properties. Mutations
MUST name the target instance/incarnation and pane generation explicitly rather
than implicitly select the active pane. Input additionally needs a current
foreground generation and bounded one-writer lease; execute and terminate are
separate rights. A retry ID MUST bind authenticated actor, target generation and
payload, never a body-supplied principal. Unsupported execution remains refused.

Unconditional invariants are no names-only first-claim impersonation, no
replayable enrolment proof and no observation disclosure of bootstrap secrets.
The profile defends against other-UID impersonation, same-UID names-only
impostors without the key, forged metadata, stale generations and captured
transcripts. It does not defend against root, permitted ptrace/process-memory
access, stolen FDs/keys or deliberate owner delegation. `/proc` environment/FD
visibility follows OS access checks and may include same-UID peers. The FD
reduces accidental inheritance/exposure; it does not isolate hostile same-UID
code. An ambient owner connection has no session record or lease and is bounded
only by this UID trust boundary; a child can open one directly under default-open.
Restricted mode is not an OS sandbox either.
Allocated names intentionally reveal their diagnostic base36 UID prefix and
same-host UID activity. This is an accepted residual (such activity is already
visible through `/proc` subject to host policy); names are not withheld from
cross-UID discovery, but their identity-graph descriptors remain protected.

**BROKER-024 — Protected observation (intended).** Bootstrap and authenticated
session traffic MUST be classified from broker-owned command/route/record state,
not an optional caller flag. Protected requests, responses, errors, private
property/watch events and retained replays MUST NOT have payloads copied into
`noded.tap`, observe payload capture or general diagnostic logs, regardless of
observer allowlisting or requested body mode. Classification MUST survive
response correlation and forwarding, and MUST be persisted with each retained
topic snapshot for replay after its publishing route is gone. Omission MUST precede
enqueue; subscribers must not receive raw material later redacted in a UI.

Tap MUST omit such envelopes entirely. Observe MAY emit only bounded metadata:
its sequence/time/direction/outcome, canonical endpoints, command/type,
correlation ID, byte count, RC, drop count and omission reason
`native_session_protected`. It MUST NOT copy arbitrary headers, error text,
arguments, property paths/values, keys, proofs or screen contents. Legacy
traffic retains BROKER-013/014 behaviour. Public grants/challenges/signatures
are nonsecret, but this exclusion still applies; captured public transcripts
MUST not enrol a connection.

**BROKER-025 — Acceptance fixtures (intended).** These stable fixture IDs are
requirements, not claims of existing tests or passing evidence. All protocol
acceptance MUST exercise native ABP/noded and the real recipient enforcement
points (verb/property handlers), not broker state alone; real-child cases use a spawned Mix
with PTY stdio. Stage names denote principal foundation (S1), binding lifecycle
(S2), real launch (S3), and recipient enforcement (S4).

| Fixture ID | Intent | Completion gate |
|---|---|---|
| `p0i-01-legitimate-binding` | Discover verified Term→pane→Mix; restricted pane verb/property mutation only after binding | S4 |
| `p0i-02-names-only-impostor` | Pre-claim public names without key, same and other UID; preserve rightful reservation | S2, real child S3 |
| `p0i-03-wrong-scope` | Wrong pane, role, Term or UID cannot bind or consume the rightful grant | S2, real child S3 |
| `p0i-04-replay-resumption` | Captured proof fails on another channel/challenge and after exit/recreation/restart; fresh re-proof succeeds | S3 |
| `p0i-05-multi-owner-allocation` | Two Terms per each of two UIDs, multiple panes, PID reuse/container collisions; no replacement | S3 |
| `p0i-06-envelope-forgery` | Forged sender, case-variant principal/origin and response ID cannot impersonate or consume a pending reply; real replies work | S1, regression S4 |
| `p0i-07-recipient-policy` | Default-open ambient owner success, including a child's independent connection; pre-enrolment, sibling-scope and ungranted denial under restricted policy; cross-UID denial under both; actual verb/property gates | S4 |
| `p0i-08-revoke-races` | Child exit, pane close, Term exit, stale cleanup and broker bounce in flight; bounded revoke, no reused-name/PID rights | S4 |
| `p0i-09-observation-confidentiality` | Concurrent tap, allowed observe and logs disclose neither bootstrap secrets nor protected payloads; transcript cannot bind | S2 bootstrap, S4 payloads |
| `p0i-10-bootstrap-bounds` | Missing/wrong/expired grant, broken FD, cancelled launch, collision, outage and quota exhaustion clean up; no unauthenticated controls | S4 |

The suite MUST additionally assert Term-owned lifetime: ending a login that
stops Term revokes its bindings; a deliberately lingering Term does not falsely
claim logout revocation. Multi-UID/PID-namespace fixtures need an isolated
privileged harness; stub brokers do not establish real credentials, restart
or PTY launch acceptance. Skipped prerequisites MUST NOT be reported as passes.

## Evidence and acceptance

Source: [broker](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/crates/cosmix-noded/src/noded.rs),
[topics](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/crates/cosmix-noded/src/subscription.rs),
[spec distribution](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/crates/cosmix-noded/src/spec.rs).

Acceptance requires isolated-broker tests for spoofed identities/origin,
registration collisions, correlation, reserved topics, replay/clear/stale
expiry, full/closed queues, filtered counts, observation redaction and stop
fencing. Distribution cutover must exercise every legacy ID and new name,
file selection, startup publication, revision metadata and confidentiality.
External federation, wildcard topics, producer ownership locks, persistent
topic history and automatic producer launch remain outside this profile.
