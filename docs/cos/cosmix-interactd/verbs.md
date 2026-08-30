# cosmix-interactd Bus verbs

This page describes the Bus commands served by `cosmix-interactd`.

## General contract

The daemon registers as `interact` by default. `--bus-service` changes the
registered service name. The commands themselves retain their `interact.*`
names.

Request arguments are resolved in this order:

1. JSON in the `args` header.
2. A non-null structured Bus `args` value.
3. JSON in the raw command body.

Successful application responses use return code `0`. Request, ownership,
admission, and routing errors use return code `10` with a JSON error body.

All commands require a canonical, registered local Bus caller. Anonymous
callers and wire-asserted remote identity or permission claims are rejected.

## `interact.notify`

Accepts a `NotifyCreateRequest`, records `queued`, and attempts to reserve a
delivery slot without blocking.

The notification request surface used by the daemon includes:

| Field | Use |
|---|---|
| `summary` | Required human-visible summary. Empty or whitespace-only values are rejected. |
| `body` | Optional notification body. |
| `urgency` | Requested `low`, `normal`, or `critical` urgency. |
| `icon` | Optional supported Lucide icon reference. |
| `timeout_ms` | Optional presentation timeout in milliseconds. |
| `category` | Optional freedesktop category. |
| `actions` | Action buttons with a key, label, and optional Bus dispatch target. |
| `dedupe_key` | Optional caller-scoped coalescing key. |
| `owner_token` | Existing token required when a dedupe key replaces a live notification. |

A newly accepted notification returns:

```json
{
  "handle": "<opaque-handle>",
  "owner_token": "<owner-capability>"
}
```

The handle identifies the record and forms one property-path segment. The
owner token authorises later mutation and is never exposed through properties.

Rate limiting is a successful soft outcome. It returns `throttled: true` and
the local `origin`; it creates no delivery job.

A dedupe hit reuses the existing handle and requests an in-place replacement.
It requires both the creating caller identity and the existing owner token.

Action dispatch targets are validated asynchronously against the Bus service
registry before the sink shows the notification. Failure changes the queued
record to `failed`.

## `interact.update`

Re-renders a live notification in place. The request includes:

- `handle`
- `owner_token`
- notification request fields

The handle must exist, remain non-terminal, and belong to the calling service.
The owner token must match the token returned by `interact.notify`.

Updates reject a non-empty `actions` list. Existing action routing remains
attached to the notification. The sink can re-render the summary, body,
effective urgency, icon, timeout, and category.

The update uses the same per-origin rate bucket as notify. An accepted update
moves the record to `queued` and returns its handle and new state.

A throttled update leaves the prior presentation unchanged and returns its
handle, `throttled: true`, and the current state.

Unknown handles, terminal records, invalid notification fields, unsupported
icons, and ownership failures return code `10`.

## `interact.dismiss`

Closes a notification and marks it `dismissed`. The request requires `handle`
and `owner_token`.

The handle must belong to the calling service and the owner token must match.

Dismiss is idempotent for a terminal record. A first dismiss returns the handle
and `dismissed` state. A repeated dismiss returns the existing terminal state
and performs no second close.

## Action dispatch

An action contains a button key and label. `on_invoke`, when present, selects a
target Bus service and verb.

When the desktop reports an action, the record becomes `action_invoked`. The
daemon sends this argument object to the selected service and verb:

```json
{
  "handle": "<opaque-handle>",
  "key": "<action-key>"
}
```

An action without `on_invoke` still becomes terminal but sends no Bus command.

Clicks observed while the broker is disconnected enter a bounded FIFO. When
the queue is full, the oldest intent is dropped. Each action is attempted once;
an error or timeout after the attempt starts is not retried.

## Property commands

The daemon implements SPEC-07 L2 property conformance through:

- `interact.props.get`
- `interact.props.list`
- `interact.props.describe`
- `interact.props.watch`

The property tree is rebuilt from the in-process interaction records for each
request.

### Property paths

| Path | Type | Meaning |
|---|---|---|
| `lifecycle.props_level` | string | Conformance level, always `L2`. |
| `lifecycle.event_seq` | number | Current daemon-session lifecycle sequence. |
| `stats.live` | number | Count of non-terminal notifications. |
| `stats.total` | number | Count of retained live and terminal notifications. |
| `notifications.<handle>.origin` | string | Broker-authenticated creating service. |
| `notifications.<handle>.state` | string | Current lifecycle state. |
| `notifications.<handle>.summary` | string | Human-visible summary. |
| `notifications.<handle>.created_at_ms` | string | Unix epoch milliseconds, encoded as a precision-safe string. |
| `notifications.<handle>.urgency` | string | Effective urgency after policy. |
| `notifications.<handle>.dedupe_key` | string | Coalescing key, present only when supplied. |

The owner token and internal desktop identifiers are not properties.

### States

Property state values are:

- `queued`
- `shown`
- `dismissed`
- `expired`
- `action_invoked`
- `failed`

### Watch discovery

`interact.props.watch` returns a connection-scoped discovery object containing:

```json
{
  "topic": "interact.props.changed",
  "queue_capacity": 256,
  "event_sequence": "daemon_session_monotonic",
  "event_seq": 0,
  "loss_signal": "gap_and_lost_count",
  "bootstrap": "subscribe to the topic on this connection, then read interact.props.get"
}
```

The current `event_seq` value varies with daemon-session state.

Watchers subscribe first, then read `interact.props.get`. Each normal event
describes one `notifications.<handle>.state` transition and includes `old`,
`new`, `seq`, and `gap: false`.

The topic publication is not retained. Sequence numbers increase within one
daemon process.

If bounded event delivery loses transitions, the daemon discards older queued
events and emits a gap frame:

```json
{
  "seq": 42,
  "gap": true,
  "lost_count": 3,
  "cause": "publisher.loss"
}
```

After a gap, a watcher reads a fresh property snapshot and resumes after the
reported sequence watermark.
