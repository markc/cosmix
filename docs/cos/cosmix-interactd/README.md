# cosmix-interactd

`cosmix-interactd` is the headless CTK interaction daemon for passive
notifications. It registers the Bus `interact` service, applies notify.v1
policy, queues accepted work, and delivers notifications through a desktop or
recording sink without rendering its own interface. In the
`bus <- mix <- cos` dependency chain it is a `cos` daemon: it depends directly
on Bus crates and other `cos` crates, with no direct dependency on Mix.

## Synopsis

```text
cosmix-interactd serve [--bus-service <name>] [--sink <sink>]
```

## Description

The daemon owns Bus transport, wall-clock sampling, opaque handle creation,
owner-token creation, asynchronous delivery, action dispatch, and the live
interaction state.

Notification policy is delegated to `cosmix-interaction-broker`. Wire request
and response types come from `cosmix-interaction-schema`.

The daemon is renderless. It does not depend on Bevy and does not draw a CTK
surface.

The request path records accepted notifications as `queued` and returns before
desktop delivery. Registry lookup, sink I/O, and action-listener setup run on a
bounded worker.

The daemon reconnects to the Bus broker after connection loss. Backoff starts
at one second and is capped at 60 seconds.

## What it provides

| Component | Purpose |
|---|---|
| Bus command loop | Registers a service and routes notify.v1 and property commands. |
| Interaction state | Stores live and terminal records, ownership, revisions, routes, and lifecycle transitions. |
| Delivery worker | Serialises sink operations and performs deferred action-target validation. |
| `FreedesktopSink` | Sends desktop notifications through `org.freedesktop.Notifications`. |
| `RecordingSink` | Logs delivery intent without requiring a desktop notification daemon. |
| Property surface | Exposes an L2 read-only view under `interact.props.*`. |
| Action worker | Delivers selected action callbacks through Bus. |

The crate is a binary package. Its `state`, `sink`, and `props` modules are
internal implementation modules rather than a public Rust library API.

## Bus surface

The default Bus service name is `interact`.

| Command | Operation |
|---|---|
| `interact.notify` | Accept and queue a new notification or an owned dedupe replacement. |
| `interact.update` | Re-render an owned live notification. |
| `interact.dismiss` | Close an owned notification and mark it terminal. |
| `interact.props.get` | Read the full property snapshot or one path. |
| `interact.props.list` | List readable property paths. |
| `interact.props.describe` | Describe a property path. |
| `interact.props.watch` | Discover the lifecycle event topic and sequence watermark. |

See [Bus verbs and properties](verbs.md) for request, response, lifecycle, and
watch details.

## Command-line interface

### `serve`

Registers the Bus service and runs until the process is stopped.

| Option | Default | Meaning |
|---|---|---|
| `--bus-service <name>` | `interact` | Bus service name registered by the daemon. |
| `--sink <sink>` | `freedesktop` | Delivery backend: `freedesktop` or `recording`. |

The crate declares no daemon-specific configuration file, environment
variable, or Cargo feature. Bus connection selection is delegated to
`cosmix-lib-config` client helpers.

## Delivery sinks

The `freedesktop` sink is the normal delivery path. It sends summary, body,
effective urgency, category, timeout, resolved icon, and action buttons to the
desktop notification service.

It retains the sending connection for each live notification. This permits
same-connection action and close signals, in-place replacement by desktop ID,
and explicit close by handle.

The `recording` sink writes show, replace, and close intent to standard error.
It reports delivery as shown but has no desktop ID and therefore creates no
action listener.

A missing desktop notification daemon marks the record `failed`; it does not
stop `cosmix-interactd`.

## Notification lifecycle

A record starts in `queued`. Successful sink delivery changes it to `shown`.
It then reaches one terminal state:

- `dismissed`
- `expired`
- `action_invoked`
- `failed`

Updates move a live record back to `queued` while the replacement is pending.
Delivery completions and desktop signals carry a revision, so stale work cannot
rewrite or invoke a newer generation.

The state retains at most 256 terminal records for property snapshots. Live and
queued records are not evicted by that limit.

## Ownership and callers

New notifications receive an opaque handle and an independent owner token.
The owner token is required for update, dismiss, and dedupe replacement. The
creating local Bus service must also match.

Owner tokens are compared at fixed width and are not published through the
property surface.

Only requests from a canonical, registered local Bus service are admitted.
Anonymous callers and requests carrying wire-asserted remote identity or
permission headers are rejected.

## Actions

Each action has a key and label. An optional dispatch target names a Bus
service and verb.

Dispatch targets are checked against the service registry after a notification
has been queued and before it is shown. An invalid or unavailable target marks
delivery `failed`.

Selecting an action marks the notification `action_invoked`. If the action has
a dispatch target, the daemon sends the target:

```json
{
  "handle": "<opaque-handle>",
  "key": "<action-key>"
}
```

An ambiguous action send is not retried, preserving at-most-once dispatch.

## Icons and timeouts

notify.v1 accepts Lucide catalogue keys that the daemon can map to
freedesktop theme icon names. Unknown Lucide keys and emoji icons are rejected.

An omitted timeout uses the desktop default. A timeout of zero, or a timeout
above one hour, is clamped to one hour.

## Capacity and failure behaviour

| Resource | Capacity or deadline |
|---|---|
| Delivery command queue | 256 |
| Desktop signal queues | 128 each |
| Action delivery queue | 128 |
| Property event queues | 256 |
| Ambiguous show tombstones | 256 |
| External I/O deadline | 5 seconds |
| Maximum presentation timeout | 1 hour |

Queue admission failure returns an application error and does not mutate
notification state.

Property event loss is explicit. The publisher emits a gap frame with a
sequence watermark and loss count, allowing watchers to discard stale events
and reseed from a snapshot.

## Build

```text
cargo build -p cosmix-interactd
```

The build script emits standard Cosmix build provenance through
`cosmix-lib-buildinfo`.
