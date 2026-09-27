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
`path`, `size`), files sorted by UTF-8 byte order of the path, no trailing newline. String
scalars use JSON escaping; identity is BLAKE3 of those exact encoded bytes.
Origin, instance, creation time and collection do not participate in identity.

Limits: 1000 files, 128 KiB canonical UTF-8 bytes, 1024 UTF-8 bytes per path.
Paths reject Unicode control characters, backslashes, colons, empty components,
absolute paths, `.`/`..`, duplicates and file/directory prefix collisions.
No MIME, times, permissions, xattrs, links or empty directory preservation.
Case variants and NFC/NFD spellings are distinct paths; there is no Unicode
normalisation or case folding. Format characters (Unicode Cf) are allowed;
control characters (Cc) are not. Components over 255 UTF-8 bytes can pass
manifest validation but fail during creation on Linux restore filesystems;
that leaves only the partial directory, never a published complete restore.
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
lock prevents another process owning the same catalogue (`STORE_LOCKED`).
Configuration refusals use `STORE_CONFIG`: the resolved state root must be
nonempty, and the home-based fallback requires a nonempty `HOME`. An explicit
state root (including systemd's `STATE_DIRECTORY`) does not require `HOME`.

SQLite schema 1 uses WAL, synchronous FULL, foreign keys, short transactions
and explicit handle closure. Manifest JSON is TEXT, never a SQL BLOB. It
records collections, snapshots, snapshot-object membership and commit intents.
Rollback is best-effort on an error path so a failed BEGIN or I/O operation
reports its original error rather than a secondary rollback error.
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
| `stored.work` | none | Idempotent mesh-open kick; `{}` |

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
Manifest upload blocks the whole citizen during each HTTP call. Buffered
control calls have a 30 s total timeout; streaming calls default to a 30 s
**idle** timeout and no client total deadline, so continuing progress can block
longer. The store client's 30 s RPC timeout bounds its wait for a reply, not
the citizen's work or a streaming transfer. A timed-out caller must use the
durable status/recovery protocol rather than infer failure or cancellation.

Any mesh caller may kick `stored.work`; it is fenced and only one worker
owns the current epoch. Request deliveries (`headers.type == "request"`)
receive rc 0 and `{}` after draining, or immediately if a worker is already
active. Local `task_start` continuations have no request context and never
call `reply()`. A long drain can outlast the requester's timeout; use durable
commit status to determine the outcome. A retired worker clears its busy flag only if it
still owns that epoch. Escaping infrastructure failures (including connection
or failed-state-write errors) get at most three retries, separated by 60 s.
This is an error backstop, not a poll. After exhaustion, fix the underlying
fault and kick `stored.work` or restart to recover the pending intent.

Failed work may retain partial pins and upload receipts. This is intentional
until a separate release policy is designed. Blobd quota counts unique bytes
per owner, including manifest bytes, and pending upload reservations; it is
not the old pilot's logical snapshot quota. This is an explicit policy change.

## Errors

Citizen refusals use rc 10 and `{error_code,message}`. A failed durable job
stores and emits an `error` string in `CODE: message` form; the client prints
the same form on stderr and exits nonzero. Wrapped Bus/blob failures retain
their upstream reply in the message. Filesystem, JSON, SQLite and transport
builtins may also surface their own native error codes.

| Code | Meaning |
|---|---|
| `STORE_MISSING` | A referenced object is absent before pinning |
| `STORE_SIZE` | Object/reference sizes disagree |
| `STORE_IDENTITY` | Manifest, upload or restored bytes differ from the requested identity |
| `STORE_DB` | Catalogue schema, binding, durability or canonical-data error |
| `STORE_SCHEMA` | Wrong version, field set, or object/list shape |
| `STORE_INTEGER` | Invalid integer or value outside the format's exact range |
| `STORE_PATH` | Unsafe, duplicate or conflicting manifest path |
| `STORE_LIMIT` | File count, byte, paging or walk limit exceeded |
| `STORE_CURSOR` | Invalid paging cursor type |
| `STORE_VERB` | Unknown catalogue operation |
| `STORE_COLLECTION` | Invalid collection name |
| `STORE_BLOB` | Invalid blob identifier or wrapped blobd failure |
| `STORE_NOT_FOUND` | Collection, snapshot or commit does not exist |
| `STORE_FORGOTTEN` | Tombstoned snapshot cannot be read or resurrected |
| `STORE_BUSY` | A different commit is pending |
| `STORE_SUPERSEDED` | Internal worker generation fence; retired work stops silently |
| `STORE_LOCKED` | Another process owns this catalogue/cache lock |
| `STORE_CONFIG` | Invalid target, service name, timeout, chunk or state directory |
| `STORE_SOURCE` | Missing/nonregular source or symlink/special file |
| `STORE_CHANGED` | Source changed while hashing or transferring |
| `STORE_IGNORE` | Structurally invalid ignore rule |
| `STORE_CACHE` | Cache is not a real directory or lies inside the source |
| `STORE_REMOTE` | Wrapped catalogue Bus failure |
| `STORE_COMMIT` | Commit failed or did not finish within the client's wait |
| `STORE_DESTINATION` | Restore destination already exists or its namespace changed |
| `STORE_LEGACY_HASH` | Original pilot bytes do not match their SHA-256 |
| `STORE_LEGACY_CANONICAL` | Hash-verified pilot bytes are not canonical |
| `STORE_NOT_MIGRATABLE` | Valid pilot identity cannot fit the v2 contract |
| `STORE_USAGE` | Invalid client/package arguments |
| `STORE_PACKAGE` | Package source is not a checkout |

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
Registered service names are at most 31 ASCII bytes, so a stored instance
suffix is at most 24 bytes. Client routing and citizen startup validate the
resulting service names and return `STORE_CONFIG` for invalid/oversized names;
the `.node.bus` routing suffix is not part of the registered local name.
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
exclusive process lock covers it (`STORE_LOCKED` on contention); it cannot
live inside the source tree. An empty `--cache` or an empty `HOME` when using
the default cache is refused with `STORE_CONFIG` before filesystem writes.
Only newly created cache directories are chmodded to 0700; existing directory
modes are preserved. Canonical paths are compared before creating `.lock`,
including aliases of the source root.
Target, collection and source root isolate resume records; object identity
names each record. Atomic mode-0600 records retain B0's session key, identity
and server receipt across a killed push. Retry the same command. Changed
bytes get a different record; old receipts/reservations are retained until
blobd expiry or explicit operator abort. Transfers are sequential; `--chunk`
accepts 1..8388608 bytes and `--timeout` defaults to 900 seconds.

The client subscribes before commit. Events trigger immediate durable status
checks, with a bounded backstop: first check after 2 s, then double the delay
to at most 60 s until `--timeout`. Transient status errors retain that schedule.
Noded does not federate this local subscription automatically: Mix subscribes
on its local serve connection (`cosmix-mix/src/bus.rs`, `subscribe_topic`), and
`cosmix-noded/src/subscription.rs` fans out to that broker's subscriptions.
Blob fetch completion is local to the receiving blobd; cross-node byte fetch
does not imply cross-node topic forwarding. Final status reads have at most
one second of grace after the deadline. Pending is never reported as committed. `list` follows
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
It pins the same runtime `RUST_LOG` targets as the statecache citizen.

Owners `store:<collection>` and `store-restore:<collection>` inherit blobd's
`quota_owner_default_bytes` unless an operator configures explicit
`quota_owner: store:example=10GiB` and
`quota_owner: store-restore:example=10GiB` lines on their respective blobd
instances. Set real collection limits during the separately authorised
migration/deployment; creating a collection does not create quota policy.

Nothing in B1 prunes client resume records, `STATE/manifests` or
`STATE/uploads`. Their reconciliation, along with retained pins, belongs to
the deferred retention reconciler. Blobd session expiry is a separate policy
and does not remove these catalogue/client files.

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
