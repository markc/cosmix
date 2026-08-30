# cosmix-noded Bus verbs

This page describes the Bus commands implemented by `cosmix-noded` and its built-in `mon` and `log` services.

## Conventions

Successful commands return `rc: 0`. Invalid arguments, denied operations, unknown commands, and unavailable data normally return `rc: 10`.

Arguments named as headers below are Bus headers. Commands that accept property or specification arguments also accept a JSON object in the `args` header or message body; the `args` header takes precedence.

## Node service

### `noded.register`

Registers the current WebSocket connection under the Bus `from` header. The name must not already belong to another connection.

The optional JSON body is registration provenance. Recognised provenance is stored in the service registry and returned by `noded.list`. An empty or malformed body still permits name-only registration.

The result contains the registered name.

### `noded.deregister`

Removes the service name held by the current connection and removes that connection's subscriptions. The operation is idempotent and cannot deregister another connection.

### `noded.list`

Returns a name-sorted JSON array of registered service information. Entries can include binary name, version, source revision, dirty state, build time, process identifier, start time, registration time, schema version, and metadata when supplied at registration.

### `noded.info`

Returns live node information: node name, broker address, the broker's build provenance, uptime, registered service count, and schema version.

### `noded.ping`

Returns `pong: true` and advertises the core and topic extension versions.

### `noded.peers`

Returns the local node's broker address information and the configured mesh peer roster. Each peer entry contains a name, mesh address, and broker port.

This is the routing roster. It is distinct from the signed authority inventory.

### `noded.inventory`

Returns the current authority posture.

A verified result includes the inventory epoch, recovery generation, recovery flag, mesh identifier, content hash, verifying key identifiers, adopted verify-key summaries, and member summaries.

An unverified result contains `posture: "unverified"` and a reason.

### `noded.tap`

Subscribes the connection to a live copy of every inter-service Bus message routed by this broker.

The command is restricted to same-node connections. It exposes message headers and bodies and therefore bypasses per-service read controls. Delivery is lossy when the tap consumer's outbound queue is full.

### `noded.props.get`

Reads the complete broker property snapshot or a selected property path.

### `noded.props.list`

Lists broker property paths.

### `noded.props.describe`

Describes a broker property path, including its type and metadata.

The broker always redacts sensitive values on this surface.

### `props.watch`

Subscribes the caller to `noded.props.changed`. The response includes a subscription identifier, topic name, replay flag, and sequence number.

### `noded.props.subscribe_grant`

Subscribes another registered peer to a reserved property records or audit topic, filtered to one namespace.

Required headers are `topic`, `target_peer`, and `namespace`. Only the registered owner of the reserved topic may grant the subscription. The target must be a connected, registered peer. Wildcard namespaces are not accepted.

This is a privileged substrate verb used after a property service performs its own capability check.

## Topic service

### `topic.publish`

Publishes an inner Bus message to the topic named by the required `name` header.

The optional `retain` header defaults to `true`. A retained message replaces the topic's cached snapshot. `retain: false` delivers the message without replacing that snapshot.

The inner Bus message is limited to 1 MiB and must parse successfully. Names beginning with `$` are rejected. The result contains the allocated per-topic sequence and delivered subscriber count.

The broker overwrites the reserved delivery headers `topic`, `topic_seq`, `topic_stale`, and `topic_op`.

### `topic.subscribe`

Subscribes the current peer to the topic named by the required `name` header. Repeating the same subscription is idempotent.

The result contains a subscription identifier, whether a retained snapshot was replayed, and its sequence number. A stale retained snapshot is marked with `topic_stale: true`.

Direct subscription to `<service>.props.records.changed` and `<service>.props.audit` is refused. Callers reach those topics through the owning service's property watch verbs.

### `topic.unsubscribe`

Removes the current peer's subscriptions for the topic named by the required `name` header. The operation is idempotent.

### `topic.subscriber_count`

Returns the subscriber count for the topic named by the required `name` header.

For a reserved property records or audit topic, only the registered owning service may read the count.

### `topic.list`

Returns topic metadata, optionally filtered by the `prefix` header.

Each item includes `name`, `subscribers`, `has_snapshot`, `snapshot_seq`, `snapshot_size`, `last_publisher`, and `stale`. Reserved property records and audit topics are hidden from non-owners.

### `topic.clear`

Clears the retained snapshot for the topic named by the required `name` header.

The optional `notify` header defaults to `true`. When enabled, current subscribers receive a clear operation. The result reports the delivered count.

Only the owning service may clear a reserved property records or audit topic.

### Publisher notifications

The broker sends `topic.active` to the last publisher when a topic changes from zero subscribers to one. It sends `topic.idle` when the count returns to zero. These are pushed notifications, not caller-facing RPC verbs.

## Monitor service

The `mon` service is absent when the daemon starts with `--no-monitor`.

### `mon.status`

Returns host name, system uptime, logical CPU count and usage, memory and swap totals and usage, selected disk usage, and one-, five-, and fifteen-minute load averages.

Disk reporting includes `/` and mount points below `/home` or `/data`.

### `mon.processes`

Returns processes sorted by descending CPU usage. Each entry contains process identifier, name, CPU usage, and memory usage in MiB.

The optional JSON argument `limit` controls the maximum number of entries. The default is 15.

### `mon.props.get`

Reads the complete monitor property snapshot or a selected path.

### `mon.props.list`

Lists monitor property paths.

### `mon.props.describe`

Describes a monitor property path.

The monitor property surface includes lifecycle state, system CPU and memory metrics, load averages, disk summaries, and `config.top_processes_default_limit`.

## Logger service

The `log` service is absent when the daemon starts with `--no-log`.

### `log.props.get`

Reads the complete logger property snapshot or a selected path.

### `log.props.list`

Lists logger property paths.

### `log.props.describe`

Describes a logger property path.

The logger property surface includes its output path, lifecycle state, tap subscription state, observed event count, and bytes written.

## Specification service

### `spec.get`

Reads a specification chapter from the configured specification directory.

Pass either `chapter` or `name` in the JSON arguments. `chapter` accepts a non-negative number or numeric string. `name` is a single filename with an optional `.md` suffix; path separators, absolute paths, empty names, and parent references are rejected.

Scalar frontmatter fields are copied to response headers. The Markdown prose is returned as the body. Missing configuration, invalid arguments, read errors, and unknown chapters return `rc: 10`.

## UI subscription records

### `ui.subscribe`

Registers a UI event filter from `source: VALUE` and optional `action: VALUE` lines in the message body.

The command returns `rc: 5` and `registered_but_not_routed`. The subscription is stored, but UI event delivery is not implemented.

### `ui.unsubscribe`

Removes the matching UI event filter. `source` is required and `action` is optional.

## Admission frames

When admission mode is not `off`, the broker sends `noded.admit.challenge` as the first Bus frame on a new connection. A capable peer replies with `noded.admit.response` using the same correlation identifier.

The broker reconstructs and verifies the signed transcript against its current verified inventory. Observe mode records the result without denying registration. Enforce mode uses the proof result when an inter-node connection attempts `noded.register`.

These frames are part of connection admission and are not general RPC commands.
