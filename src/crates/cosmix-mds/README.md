# cosmix-mds

Per-container-set SQLite metadata + content-addressable blob store.

The crate provides the `Mds` trait (the public surface), one
production implementation `SqliteCasMds` (per-set `data.sqlite` +
box-wide `blobs.sqlite` + flat CAS dir), and a feature-gated
`MemMds` for tests. It is the storage substrate for `cosmix-maild`
and any future consumer with the same shape: immutable
content + multi-container mutable metadata + ordered access
within a container + change log.

## Trait surface

`Mds: Send + Sync` — call from any thread. The implementation uses
internal locks; concurrent calls on the same set serialize, calls
on different sets run in parallel.

| Group | Methods |
|---|---|
| Set lifecycle | `create_set`, `delete_set`, `list_sets` |
| Container lifecycle | `create_container`, `rename_container`, `delete_container`, `list_containers`, `container_status` |
| Blob CAS | `put_blob`, `get_blob`, `blob_size`, `blob_exists` |
| Item write | `add_item`, `copy_item`, `move_item`, `remove_membership`, `store_flags` |
| Item read | `fetch_item`, `fetch_item_meta`, `list_items`, `changes_since` |
| Notification | `subscribe` (in-process tokio broadcast) |
| Operational | `rebuild_index`, `verify_blobs`, `gc`, `stats`, `stats_per_set`, `export_set`, `import_set` |

Full method signatures, parameter semantics, and per-method invariants
live in the `Mds` trait docs and source. The trait is frozen across
v1; the v1.1 amendment is purely additive and lands behind
`with_set_tx` (see § Schema co-location).

## Build profiles

- default (`--features core`) — pure storage crate, no Bus
  dependencies. Locally testable without the mesh.
- `--features cosmix` — adds an Bus publisher that fans events out via
  `cosmix-lib-client` to the broker. Not enabled by default; opt in
  explicitly per consumer.
- `--features mem-store` — exposes `MemMds` for downstream tests.
- `--features _stress-helper` — internal. Builds the
  `cosmix-mds-stress-writer` helper bin used by the SIGKILL recovery
  test. Never add to default; never install.

## Operational expectations

**Lock discipline.** Per-set writes first take their connection mutex,
then queue on one fair box-wide write gate before asking SQLite for
the single `blobs.sqlite` writer lock. Direct-index writers take the
same gate before the blobs connection mutex. The full order is
per-set mutexes (sorted UUID order when there are several) → write
gate → blobs mutex → set-cache write lock; a path may skip locks it
does not need but never reverses the order. Import has no per-set
connection, so it takes the gate before the cache write lock. See
`store.rs` for the worked discipline.

**Post-commit emission.** Every change-notification (in-process
`Notifier` event and Bus `MdsEvent`) is emitted *after* the
SQLite transaction commits. A subscriber that wakes on an event
is guaranteed the change is durable on disk.

**GC quiescence.** `gc()` runs in two passes with a configurable
quiescence wait between them. Default 60s; override with the
`COSMIX_MDS_GC_QUIESCENCE_SECS` env var (integer seconds, floor 5s
— shorter values are logged at warn level and ignored). The wait
prevents a delivery in flight at the start of pass 1 from having
its blob collected before its `add_item` commits.

**`put_blob` ordering.** `put_blob` writes the CAS file *before*
`add_item` writes the corresponding `blob` row + `blob_ref` rows.
A crash between them leaves an unindexed CAS file on disk. `gc()`
sweeps the `blob` table, not the filesystem, so it cannot reclaim
these. Detect via `rebuild_index` (`orphan_blobs_found > 0`);
manual cleanup is operator-driven in v1.

**Schema co-location (v1.1 amendment).** Sidecar tables (e.g. JMAP
state) live in the same per-account `data.sqlite` and write through
`SqliteCasMds::with_set_tx` so they share the per-set transaction
with `add_item`. Never re-acquire a connection for a sidecar write
— it splits the transaction and the same-tx guarantee is lost.

## Bus event surface

`src/bus.rs` defines the Bus event taxonomy (topic constants, payload
structs, `MdsEvent` enum). The constants there are the source of truth
for downstream consumers — change them in lockstep with any
substrate-side schema changes.

## Wiring

```rust
let (mds, rx) = SqliteCasMds::open(root)?.with_bus_events();
#[cfg(feature = "cosmix")]
tokio::spawn(cosmix_mds::bus::spawn_publisher_task(client, rx));
```

Events are emitted post-commit on the durable write that makes them
true. `EventSink::none()` is the default; the broadcast channel is only
created when `with_bus_events()` is called.

## Tests

```sh
# default suite (includes the 100-thread concurrency stress)
cargo test -p cosmix-mds

# crash-recovery stress (Unix-only; spawns the helper bin)
cargo test -p cosmix-mds --features _stress-helper --test recovery
```

Both stress tests honor `MDS_STRESS_ITERS` (default 10) so CI can
keep the wall-clock floor sane while local pressure-testing can
crank it.

The crash-recovery stress test is the proof point that the
systemd-restart path is safe (SPEC 08 §8.6.2 step 1) —
`SqliteCasMds::open` completes WAL recovery without panic
regardless of where SIGKILL landed in the `add_item` loop, and
`rebuild_index()` returns clean per-container invariants
(`max(membership.seq) < next_seq`, `exists_count` matches actual
membership rows).
