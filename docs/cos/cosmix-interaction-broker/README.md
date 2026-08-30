# cosmix-interaction-broker

`cosmix-interaction-broker` is the headless decision core for `notify.v1` notifications handled by an `interact.*` broker. It validates requests, stamps caller provenance, applies per-origin throttling, coalesces repeated notifications, clamps remote urgency, and enforces action dispatch registration. It belongs to the `cos` layer of the `bus <- mix <- cos` dependency chain and has no direct Bus, Mix, mesh, Bevy, clock, or random-number dependency.

## Role

The crate contains policy only. It accepts a validated notification request, caller information, a timestamp, a fresh handle, and optionally a service-registry view. It returns a decision for a wrapper binary to execute.

The crate does not:

- register Bus services or verbs;
- deliver desktop notifications;
- perform mesh capability checks;
- store interaction records;
- mint notification handles;
- read a clock;
- render user interface elements.

Those operations remain the responsibility of the wrapper. Injecting time, handles, and registry state keeps the decision engine deterministic and unit-testable.

## Dependency position

The only runtime dependency is the sibling `cosmix-interaction-schema` crate. That crate supplies the request, record, handle, state, urgency, and validation types used by this broker.

`serde_json` is a development dependency only.

The crate defines no Cargo features.

## Decision flow

`NotifyBroker::accept` processes a notification in this order:

1. Validate the `NotifyRequest` through the schema crate.
2. Reject actions which dispatch to an unregistered service.
3. Stamp the origin from the caller identity.
4. Look up an existing per-origin deduplication slot.
5. Consume one token from the origin's rate-limit bucket.
6. Clamp a remote `Critical` request to `Normal`.
7. Build a queued `NotifyRecord`.
8. Return a delivery, throttle, or rejection decision.

`NotifyBroker::accept_queued` omits the service-registry lookup. It is intended for callers which validate dispatch targets later on a delivery worker.

Fresh notifications and coalescing replacements consume the same per-origin rate budget.

## Broker

### `NotifyBroker`

`NotifyBroker` owns the origin policy, rate limiter, and deduplication table.

`NotifyBroker::v1()` creates the version 1 policy:

- reserved-origin allowlist empty;
- burst capacity of five notifications per origin;
- refill rate of one token per second per origin;
- empty deduplication state.

`NotifyBroker::new(origin, rate)` accepts an explicit `OriginPolicy` and `RateConfig`.

`origin_policy()` returns a read-only view of the active origin policy.

`accept(req, caller, now_ms, fresh_handle, registry)` performs synchronous schema and dispatch-target validation before applying broker policy.

`accept_queued(req, caller, now_ms, fresh_handle)` applies queue-time policy without consulting a service registry.

`try_consume(origin, now_ms)` spends one token for a mutation of an existing notification. Updates and notification deliveries can therefore share one budget.

`retire(handle)` releases any deduplication slot associated with a terminal notification. It deliberately leaves rate-limit state intact.

## Callers and origin labels

### `Caller`

`Caller` describes the registered caller identity and whether the request came from a remote mesh peer.

- `Caller::local(from)` identifies a registered local caller.
- `Caller::anonymous()` identifies a local caller without a durable principal.
- `Caller::remote(from)` identifies a registered remote caller.

The request cannot supply its own origin label.

### `OriginPolicy`

`OriginPolicy::resolve` derives the displayed origin from the registered caller identity. Missing, empty, and whitespace-only identities resolve to `anonymous`.

The reserved labels are:

- `system`
- `cosmix`
- `root`

Reserved-label comparison is case-insensitive. Under `OriginPolicy::v1()`, a caller whose identity collides with a reserved label is stamped `anonymous`.

`grant_reserved(identity, label)` provides a post-version-1 path for granting a specific identity one reserved label. It rejects non-reserved labels.

`may_present_as(identity, label)` tests whether an identity may use a label. Non-reserved labels require no grant.

The crate exports `ANONYMOUS`, `RESERVED_LABELS`, and `is_reserved`.

## Decisions and rejection

### `NotifyDecision`

`NotifyDecision` has three variants:

| Variant | Meaning |
|---|---|
| `Deliver { record, replaces }` | Deliver and store the queued record. `replaces` identifies an existing notification to replace. |
| `Throttled { origin }` | Drop an over-budget delivery attempt and report the stamped origin. |
| `Rejected(reason)` | Refuse the request before delivery. |

A delivered record contains the broker-selected handle, stamped origin, `Queued` state, supplied creation timestamp, summary, effective urgency, optional urgency override, and optional deduplication key.

### `RejectReason`

`RejectReason::Invalid` wraps a schema `ValidationError`.

`RejectReason::UnregisteredDispatch` reports the zero-based action index and target service for the first action whose dispatch target is absent from the registry view.

### `ServiceRegistry`

`ServiceRegistry` contains one method:

```rust
fn is_registered(&self, service: &str) -> bool;
```

Any `Fn(&str) -> bool` implements the trait.

`validate_dispatch_targets(req, registry)` exposes dispatch-target validation separately. Actions without an `on_invoke` dispatch require no service registration.

## Deduplication

`DedupeTable` maps each `(origin, dedupe_key)` pair to one live `NotifyHandle`.

- `new()` creates an empty table.
- `lookup(origin, key)` returns the current handle for a slot.
- `record(origin, key, handle)` records a handle and returns any displaced handle.
- `forget_handle(handle)` removes every slot containing that handle.

The same deduplication key used by two origins occupies two independent slots.

When `NotifyBroker` coalesces a notification, the existing handle remains stable. The supplied fresh handle is ignored, and `NotifyDecision::Deliver.replaces` contains the existing handle.

Retiring the handle frees the slot. A later request with the same origin and key is then a fresh delivery.

## Rate limiting

`RateLimiter` maintains independent token buckets by origin.

`RateConfig` contains:

| Field | Meaning | Default |
|---|---|---|
| `capacity` | Maximum tokens held by one bucket | `5.0` |
| `refill_per_sec` | Tokens restored per second | `1.0` |

`RateLimiter::new(config)` creates an empty limiter with the supplied parameters.

`try_consume(origin, now_ms)` spends one token and returns whether delivery may proceed. A new origin starts with a full bucket.

Timestamp regressions contribute no refill and do not move the bucket's high-water mark backwards.

Buckets idle for more than 15 minutes are removed. The limiter retains at most 4,096 buckets and evicts the oldest bucket, with origin name as the deterministic tie-breaker, when a new origin would exceed that bound.

`forget(origin)` removes one bucket. Its next request starts with a full bucket.

## Remote urgency

A local request retains its requested urgency.

A remote request with `Critical` urgency is delivered as `Normal`. The record sets both `effective_urgency` and `urgency_override` so the wrapper can preserve the policy result.

Other remote urgency values pass through unchanged.

## Example

```rust
use cosmix_interaction_broker::{Caller, NotifyBroker, NotifyDecision};
use cosmix_interaction_schema::{NotifyHandle, NotifyRequest};

let mut broker = NotifyBroker::v1();
let request = NotifyRequest::new("Build complete");
let registry = |service: &str| service == "alpha";

let decision = broker.accept(
    &request,
    Caller::local("alpha"),
    1_000,
    NotifyHandle("notification-1".into()),
    &registry,
);

assert!(matches!(decision, NotifyDecision::Deliver { .. }));
```

The caller supplies the timestamp and freshly minted opaque handle. Production wrappers should supply their live service-registry view when using `accept`.
