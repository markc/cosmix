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
The pilot already used the same 1024-byte path grammar, 1000-file limit and
128 KiB canonical-byte limit; its sizes were u64 with no v2 aggregate bound.
Legacy validation preserves decimal u64 tokens before Mix's floating-point
JSON conversion. `assess_legacy(raw,sha256)` first verifies raw identity and
pilot validity, then reports `identity_verified:true,migratable:false,reason`
when a v2 rewrite would exceed its limits. `verify_legacy` exposes that outcome
as `STORE_NOT_MIGRATABLE: identity verified, not migratable: …`. No old integer
is rounded into a new manifest, and old array order remains significant.

## Tests

From the checkout, without root or a broker:

```text
mix src/crates/cosmix-blobd/mix/store_manifest_test.mix
mix src/crates/cosmix-blobd/mix/store_catalogue_test.mix
mix src/crates/cosmix-blobd/mix/store_commit_test.mix
mix src/crates/cosmix-blobd/mix/store_client_test.mix
mix src/crates/cosmix-blobd/mix/store_worker_test.mix
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
| `stored.work` | none | Idempotent mesh-open kick; `accepted`, `busy` |

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

Any mesh caller may kick `stored.work`; it is fenced and only one worker
owns the current epoch. A retired worker clears its busy flag only if it
still owns that epoch. Escaping infrastructure failures (including connection
or failed-state-write errors) get at most three retries, separated by 60 s.
This is an error backstop, not a poll. After exhaustion, fix the underlying
fault and kick `stored.work` or restart to recover the pending intent.

Failed work may retain partial pins and upload receipts. This is intentional
until a separate release policy is designed. Blobd quota counts unique bytes
per owner, including manifest bytes, and pending upload reservations; it is
not the old pilot's logical snapshot quota. This is an explicit policy change.

## Client

Run the shipped `mix/store.mix` beside the libraries:

```text
mix store.mix --node archive push source ./source
mix store.mix --node archive list source
mix store.mix --node archive status source b3:…
mix store.mix --node archive restore source b3:… ./new-restore
```

Without `--node`, the target is local. `--instance two` selects both
`stored-two` and `blobd-two`; `--local-blobd` selects the restore receiver
(default `blobd`). Discovery checks that stored is bound to that target.
Restore resolves each reference's origin from the target's `blob.stat`,
calls `blob_fetch_wait` sequentially with an explicit restore owner, then
downloads from the local lane with `expect_blake3`, no-clobber publication,
size verification and a final BLAKE3 check. The destination must not exist;
its parent must exist. Files are verified inside a sibling
`.<name>.partial-<uuid>` directory; only a completely verified tree is renamed
onto the requested name. Failure leaves that visibly partial directory. Keep
the destination parent free of concurrent namespace writers; Mix's rename is
atomic but does not offer `RENAME_NOREPLACE`. Restore pins
under `store-restore:<collection>` are retained in this arc too.

Push walks regular files only, hashes explicitly with BLAKE3 and uses
`blob.has` only to choose transfers. Present objects are pinned and size
checked; missing objects use durable upload sessions. Each path is rehashed
after transfer, then the whole inventory is compared again before commit.
Keep the source quiescent: this detects changes, but is not a filesystem
snapshot or protection against hostile concurrent namespace replacement.
Empty directories and file metadata are not archived.

`.storeignore` contains literal root-relative paths or trailing-slash subtree
rules, one per line; blank lines and `#` comments are ignored. Glob and
negation characters are literal, never operators. Rules require only relative
nonempty components without `.` or `..`, so an otherwise unrepresentable
filename can be excluded before manifest path validation.
The file itself is included unless `.storeignore` is
listed. Excluded entries are not inspected. The control file is at most
64 KiB and a walk inspects at most 10000 non-excluded entries, besides manifest limits.

The default cache is `~/.cache/cosmix/store/` (override `--cache`). One
exclusive process lock covers it; it cannot live inside the source tree.
Only newly created cache directories are chmodded to 0700; existing directory
modes are preserved. Canonical paths are compared before creating `.lock`,
including aliases of the source root.
Target, collection and source root isolate resume records; object identity
names each record. Atomic mode-0600 records retain B0's session key, identity
and server receipt across a killed push. Retry the same command. Changed
bytes get a different record; old receipts/reservations are retained until
blobd expiry or explicit operator abort. Transfers are sequential; `--chunk`
accepts 1..8388608 bytes and `--timeout` defaults to 900 seconds.

The client subscribes before commit, then checks durable status once after
completion or timeout. Pending is never reported as committed. `list` follows
all pages; `status` exposes the durable job including failure or tombstone.
One-shot event users finish with `quit()`; failures exit nonzero.

## Packaging and verification

The citizen and client live in `src/crates/cosmix-blobd/mix/`. They require
Mix 0.97.0 or later and blobd 0.6.1 or later. There is no new Rust binary.
The package installs the nine runtime scripts together under
`/opt/cosmix/share/cosmix/stored/`; relative `require()` paths stay intact.
Run the installed client as `mix /opt/cosmix/share/cosmix/stored/store.mix`.

Stage a package without root, account creation or service changes:

```text
mix src/crates/cosmix-blobd/mix/store_package.mix --source . --destdir /tmp/stored-package
mix src/crates/cosmix-blobd/mix/store_test.mix
systemd-analyze verify --man=no src/_etc/systemd-system/cosmix-stored.service
```

The destination must not exist. The rootless suite registers all five focused
test scripts, checks lint/syntax/version for every Mix file in this directory,
stages the assets and verifies script bytes, unit state settings and registry
projection. It needs neither cargo nor a broker.

The shipped unit is `src/_etc/systemd-system/cosmix-stored.service`;
`src/_etc/sysusers/cosmix.conf` projects registered citizen UID/GID 601,
`cosmix-stored`. An operator installs the staged assets, applies sysusers and
enables the unit in a separately authorised deployment. Systemd creates
`/var/lib/cosmix/stored` with mode 0700 and supplies `STATE_DIRECTORY`.
No `cosmix-blob` membership or CAS filesystem access is needed: even manifest
bytes go through the lane. Named instances need a separate unit/drop-in with
distinct state, `--name stored-<instance>` and `STORED_BLOBD=blobd-<instance>`.
The unit uses Wants/After dependencies so broker reconnect remains native.

The maintainer's private `stored_gate.mix` is intentionally outside this
public repository. It starts isolated named citizens on the workstation,
round-trips generated binary/empty/duplicate files through a separate fetch
receiver, kills a push after a durable partial offset, resumes the same
session/key, proves commit replay, tombstones without unpinning, checks SQLite
rows directly and restarts the catalogue. Every arm has an exact failure
token and a negative self-test. It does not exercise production deployment.

Pilot migration and cutover are separate, unimplemented slices. Migration
must first verify old raw-byte SHA-256 identities, import without deletion,
and durably map old IDs to new IDs; a second phase must restore **every**
snapshot through both clients and compare path sets, sizes and exact bytes
before cutover. A rollback set includes the pilot data and writer lock,
configuration and ACL/quota policy, old daemon **and client**, units/drop-ins
and environment, ownership/modes, deploy inventory and ID mapping. Keep that
set and the new catalogue/blob pins intact until rollback is explicitly
retired. No migration, quota configuration or fleet deployment occurs here.
