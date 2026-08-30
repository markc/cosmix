# cosmix-interaction-schema

`cosmix-interaction-schema` defines the headless serde data-transfer objects for the `interact.*` wire contract used by the ctkd ephemeral-surfaces daemon. It is the shared interaction vocabulary for non-toolkit callers, toolkit code, and the daemon boundary. In the `bus <- mix <- cos` dependency chain it belongs to the `cos` layer, but it depends only on `serde` and does not pull in Bus, Mix, Bevy, CTK, or other CosMix crates.

## Scope

The crate currently defines `notify.v1`: passive, fire-and-forget desktop notifications.

It covers the wire shapes for:

- `interact.notify`
- `interact.update`
- `interact.dismiss`

It does not define modal dialogs or other interactive surfaces.

The crate is a library. It has no binary, command-line interface, configuration file, Cargo features, or public submodules.

Notification content has no password, `sensitive`, or other confidential-field vocabulary.

## Request types

### `NotifyRequest`

`NotifyRequest` is the notification content sent by a caller.

| Field | Type | Meaning |
|---|---|---|
| `summary` | `String` | Required, non-empty single-line headline |
| `body` | `Option<String>` | Optional plain longer text |
| `urgency` | `Urgency` | Attention level; defaults to `Normal` |
| `category` | `Option<String>` | Freedesktop category hint |
| `icon` | `Option<IconRef>` | Curated catalogue reference |
| `dedupe_key` | `Option<String>` | Replacement and coalescing key |
| `timeout_ms` | `Option<u32>` | Display-timeout hint in milliseconds |
| `actions` | `Vec<NotifyAction>` | Optional action buttons |

`NotifyRequest::new(summary)` constructs a minimal request with normal urgency and no optional fields.

Call `NotifyRequest::validate()` before dispatch. Validation is structural; broker policy remains outside this crate.

The request deliberately has no `origin` field. The broker derives provenance from the registered Bus caller identity.

### `NotifyCreateRequest`

`NotifyCreateRequest` is the envelope accepted by `interact.notify`.

It flattens a `NotifyRequest` into the wire object and adds an optional `OwnerToken`.

`NotifyCreateRequest::new(request)` creates a fresh-notification envelope without an owner token. A caller replacing an existing `dedupe_key` supplies the token returned for that notification.

### `NotifyUpdateRequest`

`NotifyUpdateRequest` is the token-bearing `interact.update` request.

It contains:

- the opaque `NotifyHandle`
- the matching `OwnerToken`
- a flattened replacement `NotifyRequest`

### `NotifyDismissRequest`

`NotifyDismissRequest` is the token-bearing `interact.dismiss` request. It contains the notification handle and its owner token.

## Responses and stored records

### `NotifyResponse`

A successful `interact.notify` response returns a stable opaque `NotifyHandle` and an `OwnerToken` bearer capability for later mutation.

The handle is distinct from the Bus transport correlation ID. The broker mints it independently of request contents.

`NotifyHandle::as_str()` and `OwnerToken::as_str()` expose the wrapped string by reference.

The owner token is not notification content. It must not be logged, rendered, or projected through properties.

### `NotifyMutationResponse`

An accepted update or dismissal returns a `NotifyMutationResponse` containing the handle and current `NotifyState`.

Delivery is asynchronous. `Queued` acknowledges acceptance but does not mean that a desktop notification daemon has shown the notification.

### `NotifyRecord`

`NotifyRecord` is the broker-owned stored form of a live notification.

It stores the opaque handle, broker-stamped `origin`, lifecycle state, creation time in Unix epoch milliseconds, summary, effective urgency, optional urgency override, and optional deduplication key. It does not store or expose the owner token.

## Notification vocabulary

### `Urgency`

`Urgency` has three lowercase wire values:

| Variant | Wire value |
|---|---|
| `Low` | `low` |
| `Normal` | `normal` |
| `Critical` | `critical` |

`Normal` is the default. Broker-side policy may clamp a remote caller's requested urgency; the schema type does not perform that policy check.

### `IconRef`

`IconRef` names a curated icon rather than carrying image bytes or a filesystem path.

| Variant | Meaning |
|---|---|
| `Lucide(String)` | Key in the shared Lucide icon catalogue |
| `Emoji(String)` | Forward-compatible emoji catalogue key |

The wire variant names use `snake_case`. `notify.v1` resolves Lucide icons; emoji references remain unsupported until that asset class is available.

### `NotifyAction`

An action contains a stable `key`, a human-visible `label`, and an optional `Dispatch`.

If `on_invoke` is absent, invoking the action dismisses and records it without sending a callback.

`MAX_ACTIONS` is `3`.

### `Dispatch`

`Dispatch` identifies the registered Bus service and command invoked for an action. The service is a registered service name, not a process ID. The delivery worker checks registration before showing the notification.

## Lifecycle

`NotifyState` serialises with `snake_case` names.

| State | Terminal | Meaning |
|---|---:|---|
| `Queued` | no | Accepted but not yet handed to the notification daemon |
| `Shown` | no | Presented by the notification daemon |
| `Dismissed` | yes | Dismissed by the user or `interact.dismiss` |
| `Expired` | yes | Timed out |
| `ActionInvoked` | yes | An action was invoked |
| `Failed` | yes | Delivery failed |

`NotifyState::is_terminal()` reports whether further state transitions are prohibited.

## Validation

`NotifyRequest::validate()` rejects:

- an empty or whitespace-only summary
- more than three actions
- an empty action key
- duplicate action keys
- an empty dispatch service or command
- dispatch identifiers outside the Bus wire grammars

`ValidationError` identifies the failed rule and implements `Display` and `std::error::Error`.

The public helper `is_bus_service_name()` checks the service-name grammar: 2 to 31 ASCII bytes, starting with a lowercase letter, followed by lowercase letters, digits, or hyphens.

Dispatch commands use the form `<service>.<verb>[-<noun>]`. They are lowercase ASCII, contain one dot, use hyphens between non-empty command segments, and do not accept underscores.

Validation does not check:

- caller capability
- broker-stamped origin
- rate limits
- whether a dispatch service is currently registered
- urgency clamping
- timeout policy

Those checks belong to the broker.

## Example

```rust
use cosmix_interaction_schema::{
    Dispatch, IconRef, NotifyAction, NotifyCreateRequest, NotifyRequest, Urgency,
};

let mut notification = NotifyRequest::new("Export complete");
notification.body = Some("The archive is ready.".into());
notification.urgency = Urgency::Low;
notification.icon = Some(IconRef::Lucide("circle-check".into()));
notification.dedupe_key = Some("export-job".into());
notification.actions.push(NotifyAction {
    key: "open".into(),
    label: "Open".into(),
    on_invoke: Some(Dispatch {
        service: "example-app".into(),
        verb: "document.open".into(),
    }),
});

notification.validate().expect("valid notification");
let _wire_request = NotifyCreateRequest::new(notification);
```
