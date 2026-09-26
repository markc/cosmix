# Bus verbs

`cosmix-blobd` registers the `blob.*` namespace as the `blobd` service (or `blobd-<name>` for a named instance). Metadata only: bytes never ride a Bus frame — same-node movement is `blob.path`, cross-node is the byte lane (`GET blob.url`, or `blob.fetch` to pull and pin).

## Calling convention

For a bare command, arguments are resolved in this order:

1. JSON from the Bus `args` header.
2. The command's non-null `args` value.
3. JSON parsed from the raw body.

Success uses result code `0`. Errors use result code `10` and a body shaped as:

```json
{"error":"message"}
```

Error tokens worth matching on: `not_present` (blob not held), `invalid blob id` (malformed `b3:` reference), `quota:` (an owner or total cap refusal — the message carries the numbers), `busy` (the `blob.fetch` queue is full — retry later), `vanished:` (the bytes disappeared while the pin was landing — a `blob.gc` race; retry the put).

Verbs dispatch concurrently. The data-scaled verbs — `blob.put`, `blob.gc`, `blob.list`, `blob.pin`, `blob.unpin` — are bounded by `verb_max_concurrent` (default 8): each holds one slot, not the connection, and beyond the bound it queues; its reply is late, never lost. Every other verb (`blob.fetch`'s immediate reply, `stat`, `has`, `path`, `url`, `info`, `quota`, `props.*`) takes no slot, so eight slow verbs never starve them. Replies may arrive out of arrival order; they correlate by command id, like any Bus reply.

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

The response contains `url`, `http://<lane_bind>/blob/<hex>` — exactly the path the byte lane serves (`GET` with `Range`, `HEAD`; see the README's Byte lane section). Requires the bytes to be present and `lane_bind` to be configured; the lane publishes `lane.port`/`lane.bind` props only once its socket is listening, so a remote resolves the port with a mesh-open `blob.props.get` on the origin node's `blobd` service.

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

Owner-tagged pins (`maild:acct7`, `capture:session-12`, `filesd:notes` style). Both are idempotent; the response contains `pinned`, whether a pin row actually changed. Pinning requires the bytes to be present; unpinning never does. Emit `blob.pinned` / `blob.unpinned` on a transition. Every producer of these two events — `blob.put`, `blob.pin`/`blob.unpin`, a `blob.fetch` completion — publishes the same body, `{blob, owner}`; the topic is the event type.

### `blob.list`

| Argument | Default | Constraints |
|---|---:|---|
| `owner` | unset | Restrict to this owner's pins |
| `limit` | `100` | Clamped 1 through 1000 |
| `cursor` | unset | Blob id of the last entry of the previous page |

Inventory of blobs blobd knows (attributes or pins — an orphan file with neither is invisible, which is what the restart gate asserts). The response contains `blobs` (each a reference plus its `pins`) and `next`: the blob id of the last entry of the page — the cursor for the next page — or `null` when the page is empty. A page that comes back short (fewer entries than the limit) is the last; the page after it answers `blobs: []` with `next: null`.

### `blob.quota`

| Argument | Default |
|---|---|
| `owner` | unset (all owners) |

The response contains `owners` (`<owner>: {used, limit, reserved}`) and `total: {used, limit, reserved}`. `used` sums distinct pinned blob sizes per owner; `reserved` is the in-flight upload headroom — bytes admitted to running uploads (lane `POST`/`PUT` bodies, `blob.fetch` downloads) whose pins have not landed yet. A fresh upload is refused against `used + reserved`, so concurrent uploads cannot each spend the same cap room, and the reservation releases when the pin settles or the upload aborts.

### `blob.gc`

| Argument | Default |
|---|---|
| `dry_run` | `false` |

Candidates are CAS files with mds refcount 0 (no row counts as 0), no pin, and an mtime older than the 60-second grace window. A dry run lists; a live run unlinks, drops attributes, adjusts quota and emits `blob.swept {count}`. One sweep at a time: a `blob.gc` arriving while another runs answers rc 10 `busy` at once. The response contains `dry_run`, `count`, `bytes_freed`, the swept blob ids, and `skipped` (`referenced`, `pinned`, `young`).

### `blob.info`

No arguments. Version, git sha, build time, `root`, `instance`, `lane_bind` and `counts` (`blobs`, `pins`).

### `blob.fetch`

Cross-node pull over the origin's byte lane. **Replies immediately** — never a deferred reply (the mesh response timeout is 30 s; a multi-GiB pull outlives it).

| Argument | Requirement | Notes |
|---|---|---|
| `blob` | Blob id, or a full reference object | From a reference, the `origin` (and an `instance` member, if present) drive first-try resolution |
| `from` | Optional | Node name; overrides the reference's `origin` for the first try |
| `instance` | Optional | Remote instance name; the target service becomes `blobd-<name>` |
| `owner` | The calling service (`from`) | Pin owner for this caller |

The reply is `{accepted:true, blob, origin, in_flight, queued, present}`:

- `present:true` — the CAS already had it; the caller's pin landed and this reply is the completion (`in_flight` and `queued` are false).
- `present:false, in_flight:false` — this call admitted a new fetch.
- `present:false, in_flight:true` — a fetch for this hash was already in the system (running or queued) and this call joined it: **single-flight per hash**, one download, the joiner's pin lands on completion.
- `queued` — orthogonal to `in_flight`: true when the fetch this call admitted or joined is waiting for one of the `fetch_max_concurrent` slots, false when its download holds one. A new fetch takes its slot at admission, so `in_flight:false, queued:false` means the download is running, not waiting.

The immediate reply and `blob.fetched` are **not ordered** relative to each other — a fast fetch can complete before its reply arrives. Subscribe to `blob.fetched` before sending the fetch (`blob_fetch_wait` in `mix/blob.mix` does).

Completion is the `blob.fetched` event (`retain: false`, so a late subscriber sees nothing) plus the `blob.stat` transition `present:false → true`:

```json
{"blob":"b3:…","outcome":"ok","origin_used":"alpha","size":184320}
```

`outcome` ∈ `ok · origin_unreachable · not_found_anywhere · verify_failed · quota · io` (`origin_used` is null and `error` carries the reason on every non-`ok` outcome). Quota is settled **per owner** when the pins land: an owner (submitter or joiner) whose cap refuses the pin is listed in `refused: [owner…]` and gets no pin, while the owners that fit are pinned and the outcome stays `ok`; when no owner fits, the outcome is `quota` (with `refused`). A download that outlives `fetch_deadline_secs` is `origin_unreachable` with an `error` naming the deadline. Resolution tries the origin first (props-resolved lane URL through the local noded), then falls back to a `blob.has` fan-out over `noded.peers`; `verify_failed` is terminal, never retried against another peer. Bounds, classification rules and the transfer details are in the README's [Fetching] section.

`blob.fetched` is **best-effort across a broker reconnect**: events published while blobd has no broker connection are buffered (bounded, 256 — the oldest drop beyond it) and replayed on the next connection, oldest first. Recovery when an event may have been missed: `blob.stat` showing `present:true`, and `blob_fetch_wait`'s own timeout — never polling, never a retained event.

[Fetching]: ../cosmix-blobd/#fetching
