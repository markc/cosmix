# Bus verbs

`cosmix-wgd` registers as the Bus service `wgd` and answers four read-only verbs. Request arguments and request bodies are ignored.

Every valid verb returns `rc=0`. Before the first reconcile completes, the body is:

```json
{
  "ready": false,
  "reason": "no reconcile pass has completed yet"
}
```

An unknown or non-read-only verb returns `rc=10` and names the supported verbs. There are no mutation verbs.

## `wgd.iface.status`

Returns the latest interface and reconcile summary.

| Field | Type | Meaning |
|---|---|---|
| `ready` | boolean | `true` when a snapshot exists. |
| `iface` | string | WireGuard interface read during the pass. |
| `mesh` | string | Verified inventory mesh identity. |
| `epoch` | integer | Accepted inventory epoch. |
| `self_name` | string | Local inventory member name. |
| `self_mesh_ip` | string | Local member mesh address. |
| `intended_peer_count` | integer | Number of derived peers, excluding self. |
| `live_available` | boolean | Whether `wg show IFACE dump` succeeded and parsed. |
| `live_error` | string or null | Live-read error when unavailable. |
| `synced_count` | integer | Correct live peers; zero when live state is unavailable. |
| `drift_count` | integer | Detected drift entries; zero when live state is unavailable. |
| `in_sync` | boolean | Whether live state is available and contains no drift. |
| `refreshed_at_unix` | integer | Snapshot creation time in Unix seconds. |

`in_sync` is false when live state is unavailable. A zero `drift_count` in that state does not mean that the interface is in sync.

## `wgd.peer.status`

Returns live liveness for peers that match the intended mesh address, an accepted key, and exactly the intended host route.

| Field | Type | Meaning |
|---|---|---|
| `ready` | boolean | `true` when a snapshot exists. |
| `iface` | string | WireGuard interface read during the pass. |
| `live_available` | boolean | Whether live state was read and parsed. |
| `live_error` | string or null | Live-read error when unavailable. |
| `peers` | array | Matching live peers; empty when live state is unavailable. |

Each `peers` entry contains:

| Field | Type | Meaning |
|---|---|---|
| `name` | string | Inventory member name. |
| `mesh_ip` | string | Stable mesh address. |
| `pubkey` | string | Base64 WireGuard public key installed in the kernel. |
| `state` | string | `pending`, `connected`, or `offline`. |

Peers with drift are not included in `peers`. Use `wgd.drift` or `wgd.topology.snapshot` to inspect them.

## `wgd.topology.snapshot`

Returns the complete intended peer set and any drift available from the same snapshot.

| Field | Type | Meaning |
|---|---|---|
| `ready` | boolean | `true` when a snapshot exists. |
| `mesh` | string | Verified inventory mesh identity. |
| `subnet` | string | Inventory subnet in CIDR form. |
| `epoch` | integer | Accepted inventory epoch. |
| `self` | object | Local member `name` and `mesh_ip`. |
| `intended_peers` | array | Derived peers sorted by mesh address. |
| `live_available` | boolean | Whether a live report is present. |
| `drift` | array | Drift entries, or an empty array when live state is unavailable. |

Each `intended_peers` entry contains:

| Field | Type | Meaning |
|---|---|---|
| `name` | string | Inventory member name. |
| `mesh_ip` | string | Member mesh address. |
| `allowed_ip` | string | Intended `/32` or `/128` host route. |
| `acceptable_pubkeys` | array | Base64 keys valid at the accepted epoch. |

The acceptable-key array normally has one entry. It may have two entries during a credential-overlap window.

## `wgd.drift`

Returns the dry-run reconcile result.

When live state is available, the body contains:

| Field | Type | Meaning |
|---|---|---|
| `ready` | boolean | `true`. |
| `live_available` | boolean | `true`. |
| `mesh` | string | Mesh identity carried by the report. |
| `epoch` | integer | Inventory epoch carried by the report. |
| `in_sync` | boolean | `true` when `drift` is empty. |
| `drift` | array | Complete drift entries. |

When live state is unavailable, the body contains `ready: true`, `live_available: false`, `live_error`, and an empty `drift` array. It omits `mesh`, `epoch`, and `in_sync`.

## Drift entries

### `missing`

```json
{
  "kind": "missing",
  "name": "beta",
  "mesh_ip": "192.0.2.20",
  "action_p3": "add"
}
```

### `extra`

```json
{
  "kind": "extra",
  "pubkey": "BASE64_PUBLIC_KEY",
  "allowed_ips": ["192.0.2.99/32"],
  "action_p3": "remove"
}
```

### `key_mismatch`

```json
{
  "kind": "key_mismatch",
  "name": "beta",
  "mesh_ip": "192.0.2.20",
  "live_pubkey": "BASE64_PUBLIC_KEY",
  "action_p3": "rotate"
}
```

### `allowed_ips_drift`

```json
{
  "kind": "allowed_ips_drift",
  "name": "beta",
  "mesh_ip": "192.0.2.20",
  "live_allowed_ips": ["192.0.2.20/32", "192.0.2.64/32"],
  "action_p3": "reset_allowed_ips"
}
```

### `duplicate_kernel_claimant`

```json
{
  "kind": "duplicate_kernel_claimant",
  "mesh_ip": "192.0.2.20",
  "count": 2,
  "action_p3": "operator_resolve"
}
```

The `action_p3` field describes a possible later convergence action. This crate does not execute it.

## Availability

The Bus connection runs independently of the reconcile loop. Broker outages make the verb surface unavailable but do not stop inventory verification, live reads, or snapshot replacement.

After a mid-session disconnect the Bus task reconnects with bounded exponential backoff. A session lasting at least 30 seconds resets the delay to one second.

[Back to cosmix-wgd](README.md)
