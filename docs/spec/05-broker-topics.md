---
title: Broker routing, discovery and topics
chapter: 5
version: 0.2.1
status: draft
date: 2026-09-05
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
