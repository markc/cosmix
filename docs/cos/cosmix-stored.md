# cosmix-stored

The offsite store names immutable snapshots over blobd's bytes. Its control
surface is a mesh-open Mix citizen; file bytes use blobd's discovered lane.

## Manifest v2

The only fields are `schema_version: 2` and `files`, an array of objects with
exactly `blob`, `path`, `size`. Blob IDs are lowercase `b3:<64 hex>`. Sizes and
their sum are exact nonnegative integers through 9007199254740991. One blob
cannot have conflicting sizes. Empty files and empty snapshots are valid.

`mix/store_manifest.mix` beside blobd is the shared canonicaliser. It emits
compact UTF-8 JSON, lexical field order (`files`, `schema_version`; `blob`,
`path`, `size`), files sorted lexically by path, no trailing newline. String
scalars use JSON escaping; identity is BLAKE3 of those exact encoded bytes.
Origin, instance, creation time and collection do not participate in identity.

Limits: 1000 files, 128 KiB canonical UTF-8 bytes, 1024 UTF-8 bytes per path.
Paths reject Unicode control characters, backslashes, colons, empty components,
absolute paths, `.`/`..`, duplicates and file/directory prefix collisions.
No MIME, times, permissions, xattrs, links or empty directory preservation.
Collection names match `[A-Za-z0-9_-]{1,64}`; owners are `store:<collection>`.

## Old pilot identity

Migration first verifies SHA-256 of the **original raw manifest bytes**, then
validates schema 1 and requires exact canonical-byte equality. The pilot's
file-array order is significant and MUST NOT be sorted before verification.
Only afterwards may a v2 manifest be constructed. Rewritten manifests must
pass the new byte limit; migration must never truncate or omit entries.

## Tests

From the checkout, without root or a broker:

```text
mix src/crates/cosmix-blobd/mix/store_manifest_test.mix
mix src/crates/cosmix-blobd/mix/store_catalogue_test.mix
mix src/crates/cosmix-blobd/mix/store_commit_test.mix
```

## Catalogue citizen

Run `mix --serve stored.mix --name stored`. `STORED_STATE_DIR` overrides the
state root; otherwise `STATE_DIRECTORY`, then the Cosmix/XDG state root is
used. `STORED_BLOBD` selects a **local** instance (default `blobd`). A catalogue
is bound to that instance and refuses accidental rebinding. An exclusive root
lock prevents another process owning the same catalogue.

SQLite schema 1 uses WAL, synchronous FULL, foreign keys, short transactions
and explicit handle closure. Manifest JSON is TEXT, never a SQL BLOB. It
records collections, snapshots, snapshot-object membership and commit intents.
The runtime supplies lifecycle props, HELP, INFO, QUIT and RELOAD.

| Verb | Arguments | Result |
|---|---|---|
| `store.collection.create` | `name` | Idempotent collection record |
| `store.collection.list` | `after?`, `limit?` | `collections`, `next` |
| `store.snapshot.list` | `collection`, `after?`, `limit?` | Live `snapshots`, `next` |
| `store.snapshot.get` | `collection`, `id` | Verified `id`, `manifest`, `manifest_blob` |
| `store.snapshot.forget` | `collection`, `id` | Tombstone; `pins_retained:true` |
| `store.commit.status` | `collection`, `id` | Durable state, error, times, forgotten flag |
| `store.info` | none | Counts, schema, blobd target and release policy |

Lists default to 100, accept 1..100, use exclusive lexical cursors. An exact
full final page can require one extra empty read. Unknown records return
`STORE_NOT_FOUND`; tombstoned gets return `STORE_FORGOTTEN`. Errors use rc 10
with `error_code` and `message`. All verbs are mesh-open: owner labels account
bytes and are not authenticated principals. Forget never releases blob pins.

## Commit protocol

`store.snapshot.commit {collection,manifest}` validates and canonicalises v2,
then durably records a pending intent before replying
`{accepted:true,state:"pending",collection,id,replay}`. Acceptance is **not**
publication. Only one pending commit is admitted across the catalogue;
another identity gets `STORE_BUSY`. Replaying the same collection/manifest
returns its pending or committed state. Replaying a failed job retries it;
a tombstoned snapshot refuses with `STORE_FORGOTTEN`.

A local asynchronous worker checks size and pins each distinct data object
under `store:<collection>`. It treats rc success with `pinned:false` as an
existing successful pin. `blob.pin` rechecks presence under blobd's GC lock;
`blob.has` alone cannot establish retention. The worker uploads the exact
canonical manifest through the local blobd lane with the same owner, verifies
the returned identity and size, then inserts the snapshot, distinct membership
and committed status in **one FULL SQLite transaction**. No transaction spans
Bus or lane I/O. No failure, retry or forget unpins anything.

After completion it publishes non-retained `store.commit.finished` with
`collection,id,state` and an `error` on failure. Subscribe before submission;
the authoritative fallback is `store.commit.status`. Lost events cannot lose
the result. A restart resumes pending intent. Reload replaces a durable
generation fence so an old worker cannot publish catalogue rows. Manifest
upload resume records are generation-specific to avoid concurrent writers
during reload. Async Bus waits yield; the bounded manifest HTTP transfer is
currently a blocking Mix builtin.

Failed work may retain partial pins and upload receipts. This is intentional
until a separate release policy is designed. Blobd quota counts unique bytes
per owner, including manifest bytes, and pending upload reservations; it is
not the old pilot's logical snapshot quota. This is an explicit policy change.
