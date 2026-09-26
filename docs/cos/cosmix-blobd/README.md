# cosmix-blobd

`cosmix-blobd` is the node blob store: immutable bytes by BLAKE3 hash in a content-addressed store (mds's CAS), names by owner (pin rows in a blobd-owned `blobd.sqlite`), movement by side-channel (a byte lane beside the Bus). References — `{"blob":"b3:<64 hex>","size":N,"mime":"…","origin":"<node>"}` — cross the Bus; bytes never do. It belongs to the `cos` daemon layer in the `bus <- mix <- cos` dependency chain and builds on `cosmix-mds`'s blob layer.

## Synopsis

```text
cosmix-blobd [-c CONFIG]
```

The package builds the `cosmix-blobd` binary and the `cosmix_blobd` library. The library follows the core-first pattern (`cosmix-powerd`'s shape): the store core is pure and unit-tested with the `cosmix` feature off; the Bus citizen and the byte lane exist only under that feature.

One instance owns one mds root, enforced by an exclusive `flock` on `<root>/.blobd.lock` at open — a second instance on the same root exits with status 2. That lock is what enforces "one GC owner per root". A named instance (`name:` in the config) registers as `blobd-<name>` with its own root; the verb namespace stays `blob.*` either way.

## Configuration

Flat `key: value` file at `/etc/cosmix/blobd/config.conf.mix`, passed with `-c` (filesd style: `#` comments, blank lines, key split on the first `:`, both sides trimmed).

```text
root: /var/lib/cosmix/blobd
name: two
lane_bind: 10.42.0.5:4210
lane_max_uploads: 4
quota_total_bytes: 50GiB
quota_owner_default_bytes: 10GiB
quota_owner: maild=1GiB
quota_owner: capture=2GiB
```

| Key | Default | Meaning |
|---|---|---|
| `root` | `/var/lib/cosmix/blobd` (the unit's `StateDirectory`) | mds root this instance owns |
| `name` | unset (service `blobd`) | Instance name; Bus service becomes `blobd-<name>` |
| `lane_bind` | unset (no lane) | Byte-lane bind `<ip>:<port>`; the IP must be this node's `wg_ip` (see [Byte lane](#byte-lane)) |
| `lane_max_uploads` | `4` | Concurrent lane uploads admitted; beyond it the lane answers `503` — no queueing |
| `quota_total_bytes` | `50GiB` | Total cap on accounted (pinned) bytes |
| `quota_owner_default_bytes` | `10GiB` | Per-owner cap unless overridden |
| `quota_owner: <owner>=<bytes>` | none (repeatable) | Per-owner cap; later lines for the same owner win |

Byte values accept plain integers or a binary suffix (`KiB`, `MiB`, `GiB`, `TiB`; `KB`-style spellings are still binary). Unknown keys are ignored.

### Lane port

`lane_bind`'s port is **4210** by operator convention. No port-registry specification exists in `docs/spec/` today (the broker rides a Unix socket; the mesh listener defaults to 4200, MESH-013); this README is the record of the choice — 4210, the next number above the mesh default. The port is published as the `lane.port` prop (props-only, never the signed inventory); a future shared port-registry spec should adopt 4210 rather than renumber.

## Byte lane

The lane is the HTTP listener that moves bytes: blobs never ride a Bus frame, so cross-node reads and user-side producers (capture, webd, Thunderbird) use it. It serves only the WireGuard address — **the bind proof is fail-closed**: `lane_bind`'s IP must equal this node's `wg_ip` from `node.conf.mix` (the same source noded's `bind_is_wg` uses), never unspecified, never loopback, never another interface; a mismatch (or an absent `wg_ip`) exits with status 2 before any socket is opened. The `RestrictAddressFamilies` in the unit already allows INET for it. `lane.bind`/`lane.port` props exist only once the socket is actually listening — main binds before the citizen is constructed.

### Routes

| Route | Meaning |
|---|---|
| `GET /blob/<hex>` | Stream the blob (`<hex>` = 64 hex chars, no `b3:` prefix — exactly the path `blob.url` builds) |
| `HEAD /blob/<hex>` | `GET`'s headers without the body |
| `PUT /blob/<hex>` | Upload from a client that already knows the hash |
| `POST /blob` | Server-hashed upload (curl, browsers, FileLink style) |

Reads (`GET`/`HEAD`) answer `200` with `Content-Length`, `Content-Type` from the attributes (else `application/octet-stream`), `Accept-Ranges: bytes`, `ETag: "<hex>"` and `Cache-Control: immutable`, streamed from the CAS file — never read into memory whole. A single `Range: bytes=a-b` / `bytes=a-` / `bytes=-n` answers `206` with `Content-Range` (a past-EOF last byte clamps to EOF); a start at or past EOF, or a zero suffix, answers `416` with `Content-Range: bytes */<size>`; malformed or multi-range specs are ignored and the whole blob is served (RFC 9110). An unknown hash is `404`.

`PUT` streams the body into mds staging, hashing as it lands; on completion the landed hash must equal `<hex>` or the answer is `422` — the CAS keeps no entry for either hash and nothing stays in staging. If the CAS already holds `<hex>`, the lane answers `200` **without reading the body** (the hash is the identity; the body cannot change it) and pins it to the lane owner. `POST` is the same pipeline with the hash discovered at stream end; both answer `201` with the reference JSON on success.

### Headers

| Header | Applies to | Meaning |
|---|---|---|
| `X-Cosmix-Owner` | `PUT`/`POST` | Pin owner; otherwise `lane:<peer ip>` |
| `X-Cosmix-Mime` | `PUT`/`POST` | Recorded mime; otherwise sniffed from `X-Cosmix-Name`, else `application/octet-stream` |
| `X-Cosmix-Name` | `PUT`/`POST` | Name hint recorded in the attributes |
| `Range` | `GET`/`HEAD` | Single range, see above |

Every successful upload records attributes (`origin` = this node) and a pin, so `blob.stat`/`blob.list` see it immediately and quota accounts it.

### Caps and bounds

The total cap and the lane owner's remaining quota are enforced **mid-stream** by a byte counter on the staging write: exceeding either aborts the upload, deletes the staging file and answers `413`; a declared `Content-Length` over the cap is refused `413` before any byte is read. An idle request body (no data for 30 s) aborts with `408`. At most `lane_max_uploads` (default 4) uploads run concurrently; beyond that the lane answers `503` immediately — there is no queue (the no-poll/no-flood law).

Uploads are **restart-only** in v1: a dropped or failed upload starts again from zero. Resumable upload (offset tickets) is a named P5 requirement precisely because the offsite branch it replaces was resumable by construction.

## Storage layout

```text
/var/lib/cosmix/blobd/
├── blobs/<h2>/<h2>/<hash64>   mds CAS: immutable, sharded, BLAKE3-named
├── blobs/.tmp/                staging (emptied at every startup)
├── blobs.sqlite               mds's box-wide blob index (schema v1, untouched)
├── blobd.sqlite               blobd-owned: blob_attrs, pins, quota
└── .blobd.lock                exclusive instance flock
```

mds's `blobs.sqlite` schema is never touched (`BLOBS_LATEST` stays 1 — ADR D4): blobd holds a read-only connection to it for refcount truth and joins it against its own pin table. Attributes (mime, name hint, origin, first_put), owner pins and quota accounting live in `blobd.sqlite` beside it. Folding them into mds as a schema v2 is P4 work.

At startup blobd removes everything under `blobs/.tmp` (nothing in-flight can survive its own restart) and runs one directory-vs-database reconcile: CAS files with no mds row and no pin, older than the 60-second grace window, are logged as orphans but never deleted at startup — `blob.gc` owns deletion.

## Garbage collection and quotas

`blob.gc` sweeps CAS files whose mds refcount is 0 (no row counts as 0 — every blobd put is rowless until a set references it), that carry no pin, and whose mtime is older than the 60-second grace window (`DEFAULT_GC_QUIESCENCE`). A dry run lists candidates; a live run unlinks, drops attributes and adjusts quota accounting. Pinned blobs are never candidates.

Quotas are correctness, not authorisation: `blob.put` checks the owner cap and the total cap **before** the copy (size from `stat`), and accounts when the pin lands. `used` is the sum of distinct pinned blob sizes per owner; unpinning releases it. Idempotent re-puts of an already-pinned blob are refused at the cap like any other put (the quota check runs before the hash is known).

## Permissions

Registry UID 521 (`cosmix-blobd`), shared-credential group 522 (`cosmix-blob`, SPEC 10a v1.4.7). `blob.put {path}` is daemon-local ingest: processes that share the `cosmix-blob` group (maild, filesd). Same-node readers of `blob.path` traverse the 0750 CAS as group members. User-side and remote producers (capture, webd, Thunderbird) push bytes through the byte lane — no cross-user path read exists or is needed. Under the 2026-09-15 full-mesh-access law the verbs are mesh-open with no authorisation gates; `blob.put`'s path argument is on record as the first verb to jail if a lock is ever opted in.

## Events

All publishes are `retain: false` (noded's `topic.publish` defaults to `retain: true`, which would replay the last event to any late subscriber):

- `blob.pinned {blob, owner}`
- `blob.unpinned {blob, owner}`
- `blob.swept {count}`
- `blob.props.changed` (SPEC-07 shape; `lifecycle.generation` is transient)

`blob.fetch` is reserved and replies `not_implemented` until P1 slice 4.

## Bus interface

The verb reference is [verbs.md](verbs.md); the props surface is `blob.props.{get,list,describe,watch}` over `lane.bind`, `lane.port`, `root`, `instance`, `counts.blobs`, `counts.pins`, `quota.total.used`, `quota.total.limit` and `lifecycle.generation`.
