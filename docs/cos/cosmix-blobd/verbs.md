# Bus verbs

`cosmix-blobd` registers the `blob.*` namespace as the `blobd` service (or `blobd-<name>` for a named instance). Metadata only: bytes never ride a Bus frame — same-node movement is `blob.path`, cross-node is the byte lane (`GET blob.url`).

## Calling convention

For a bare command, arguments are resolved in this order:

1. JSON from the Bus `args` header.
2. The command's non-null `args` value.
3. JSON parsed from the raw body.

Success uses result code `0`. Errors use result code `10` and a body shaped as:

```json
{"error":"message"}
```

Error tokens worth matching on: `not_present` (blob not held), `invalid blob id` (malformed `b3:` reference), `quota:` (an owner or total cap refusal — the message carries the numbers), `not_implemented` (`blob.fetch` until P1 slice 4).

A blob reference is `{"blob":"b3:<64 hex>","size":N,"mime":"…","name":"…"?, "origin":"<node name>"}` (`name` present only when the ingester knew one; `origin` is a node name, never an IP).

## Verbs

### `blob.put`

Daemon-local ingest: hash the file at `path` streaming into the CAS (copy, reflink with soft fall-through, or hard-link), then pin it.

| Argument | Default | Constraints |
|---|---:|---|
| `path` | Required | Absolute path readable by the `cosmix-blobd` user |
| `owner` | The calling service (`from`) | Pin owner |
| `mime` | Sniffed from `name`/`path` extension | String |
| `name` | The `path` file name | String |
| `mode` | `copy` | One of `copy`, `reflink`, `hardlink` |
| `immutable` | `false` | Must be `true` when `mode` is `hardlink` |

`hardlink` aliases the source inode into the CAS and is only for publishers that promise the source path immutable from the call onward — never a filesd place. Quota (owner cap, then total cap) is checked before the copy. The reply is the reference. Idempotent: a re-put returns the same reference and pins nothing new. Emits `blob.pinned` when a pin was added.

### `blob.stat`

| Argument | Requirement |
|---|---|
| `blob` | Blob id (`b3:<64 hex>`) |

The response contains `present`, `size` (filesystem, or the last known index size when absent), `mime`, `pins` (owner list), `origin` and `first_put` (milliseconds since the epoch; the latter three are `null` when blobd holds no attributes row).

### `blob.path`

| Argument | Requirement |
|---|---|
| `blob` | Blob id |

The response contains the absolute CAS `path` for zero-copy same-node reads, or `not_present` (rc 10) when the bytes are not held.

### `blob.url`

| Argument | Requirement |
|---|---|
| `blob` | Blob id |

The response contains `url`, `http://<lane_bind>/blob/<hex>` — exactly the path the byte lane serves (`GET` with `Range`, `HEAD`; see the README's Byte lane section). Requires the bytes to be present and `lane_bind` to be configured; the lane publishes `lane.port`/`lane.bind` props only once its socket is listening, so a remote resolves the port with a mesh-open `blobd.props.get` on the origin node.

### `blob.has`

| Argument | Requirement |
|---|---|
| `blobs` | Array of blob ids |

Bulk presence check. The response contains `present` and `missing` arrays of blob ids, in input order.

### `blob.pin` / `blob.unpin`

| Argument | Requirement |
|---|---|
| `blob` | Blob id |
| `owner` | Pin owner (defaults to the calling service) |

Owner-tagged pins (`maild:acct7`, `capture:session-12`, `filesd:notes` style). Both are idempotent; the response contains `pinned`, whether a pin row actually changed. Pinning requires the bytes to be present; unpinning never does. Emit `blob.pinned` / `blob.unpinned` on a transition.

### `blob.list`

| Argument | Default | Constraints |
|---|---:|---|
| `owner` | unset | Restrict to this owner's pins |
| `limit` | `100` | Clamped 1 through 1000 |
| `cursor` | unset | Blob id of the last entry of the previous page |

Inventory of blobs blobd knows (attributes or pins — an orphan file with neither is invisible, which is what the restart gate asserts). The response contains `blobs` (each a reference plus its `pins`) and `next` (a cursor, or `null` at the end).

### `blob.quota`

| Argument | Default |
|---|---|
| `owner` | unset (all owners) |

The response contains `owners` (`<owner>: {used, limit}`) and `total: {used, limit}`. `used` sums distinct pinned blob sizes per owner.

### `blob.gc`

| Argument | Default |
|---|---|
| `dry_run` | `false` |

Candidates are CAS files with mds refcount 0 (no row counts as 0), no pin, and an mtime older than the 60-second grace window. A dry run lists; a live run unlinks, drops attributes, adjusts quota and emits `blob.swept {count}`. The response contains `dry_run`, `count`, `bytes_freed`, the swept blob ids, and `skipped` (`referenced`, `pinned`, `young`).

### `blob.info`

No arguments. Version, git sha, build time, `root`, `instance`, `lane_bind` and `counts` (`blobs`, `pins`).

### `blob.fetch`

Reserved (P1 slice 4). Replies rc 10 `not_implemented`.
