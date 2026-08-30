# cosmix-mds

`cosmix-mds` is a metadata-store library and operator binary for immutable content, mutable multi-container metadata, ordered container membership, and change logs. It stores each container set in SQLite and stores content in a shared BLAKE3-addressed blob store. In the `bus <- mix <- cos` dependency chain it belongs to the `cos` layer; the default build has no Bus or Mix dependency, while the optional `cosmix` feature connects its event stream directly to Bus libraries.

## Package

| Item | Name |
|---|---|
| Cargo package | `cosmix-mds` |
| Rust library | `cosmix_mds` |
| Operator binary | `cosmix-mds` |
| Current crate version | `0.1.5` |
| Licence | MIT |

The package also declares `cosmix-mds-stress-writer`. That binary is an internal crash-recovery test helper, is gated by `_stress-helper`, and is not part of the operator surface.

## Storage model

`SqliteCasMds` uses one metadata database per set and one shared derived blob index:

```text
<root>/
├── containers/
│   └── <set-uuid>/
│       └── data.sqlite
├── blobs.sqlite
└── blobs/
    ├── .tmp/
    └── <hh>/<hh>/<blake3-hex>
```

`data.sqlite` is the source of truth for containers, items, memberships, and change streams. `blobs.sqlite` tracks blob rows, item references, refcounts, and verification results. The blob index is rebuildable from the per-set databases and the content-addressable store.

`SqliteCasMds::open` creates the storage directories, applies blob-index migrations, discovers existing sets, and applies each set's schema migrations. Migrations use `PRAGMA user_version` and reject databases with the wrong application identifier or a schema newer than the crate understands.

Blob writes hash bytes with BLAKE3, stage them under `blobs/.tmp`, fsync the file, and hard-link the result into the sharded CAS path. Repeated writes of identical bytes are idempotent.

## Library surface

The crate root re-exports:

- `Mds`, the thread-safe storage trait.
- `SqliteCasMds`, the production SQLite and CAS implementation.
- `SqliteSetTx`, the per-set transaction handle.
- `Error` and `Result`.
- All public identifiers, records, events, tokens, scopes, and report types from `types`.

`Mds` is `Send + Sync`. Calls that use the same set serialize through a per-set lock. Calls on different sets can proceed independently. Events are published only after the transaction that created them commits.

### Trait operations

| Group | Methods |
|---|---|
| Set lifecycle | `create_set`, `delete_set`, `list_sets` |
| Container lifecycle | `create_container`, `rename_container`, `delete_container`, `list_containers`, `container_status` |
| Blob CAS | `put_blob`, `get_blob`, `blob_size`, `blob_exists` |
| Item mutation | `add_item`, `copy_item`, `move_item`, `remove_membership`, `store_flags` |
| Keywords | `store_membership_keywords`, `store_item_keywords`, `item_memberships` |
| Item lookup | `fetch_item`, `fetch_item_meta`, `find_items_by_blob_hash`, `search_items`, `list_items` |
| Change streams | `changes_since`, `changes_since_set`, `prune_changelog`, `changelog_floor` |
| Notification | `subscribe`, `subscribe_existing` |
| Maintenance | `rebuild_index`, `verify_blobs`, `gc`, `stats`, `stats_per_set` |
| Transfer | `export_set`, `import_set` |

The principal identifier wrappers are `SetId`, `ContainerId`, `ItemId`, and `BlobHash`. Ordering and change tracking use the distinct `Seq`, `ChangeToken`, `SetChangeToken`, and `ContainerChangeSetToken` types.

`Flags` stores the system-flag bitmap. Allocated bits represent seen, flagged, answered, draft, and deleted states. `Tags` stores sorted, unique user keywords.

`ItemRecord` is a per-membership view joining item metadata with its container sequence, flags, tags, and change token. It is constructed from SQLite on reads and is not a persisted wire snapshot.

## Transaction scope

`SqliteCasMds::with_set_tx` runs a closure inside one `BEGIN IMMEDIATE` transaction for a set. The set database already has `blobs.sqlite` attached, so metadata, sidecar tables, and blob references can commit atomically.

`SqliteSetTx` provides typed operations for:

- Ensuring, creating, renaming, deleting, and updating containers.
- Adding a staging item or an item with multiple memberships.
- Adding, moving, and removing memberships.
- Storing item or membership flags and keywords.
- Reading memberships, sequence validity, and set-wide changes.
- Accessing the underlying `rusqlite::Transaction` when a typed method is unavailable.

Do not call an `Mds` mutation method from inside `with_set_tx`. It attempts to re-enter the same per-set lock and opens a separate transaction. Use `SqliteSetTx` methods or its raw transaction handle.

Typed transaction operations buffer notifier and Bus events. The buffer drains after commit and is discarded on error or panic. Raw SQL through `SqliteSetTx::tx` does not create events automatically.

## Change notification

`subscribe` and `subscribe_existing` return bounded Tokio broadcast receivers keyed by set and container. `subscribe_existing` checks that both objects exist while holding the same set lock used by container deletion.

The in-process `ContainerEvent` variants are:

- `ItemAdded`
- `FlagsChanged`
- `ItemRemoved`
- `ItemMoved`

Slow receivers can observe Tokio's `Lagged` error. Notifications wake consumers; they do not provide an unbounded event log. Durable consumers use the SQLite change streams.

## Bus events

The `bus` module always defines `MdsEvent`, typed payload structures, topic constants, and `EventSink`. These types can be used as an in-process broadcast surface without enabling the Bus transport dependencies.

| Topic | Transition |
|---|---|
| `mds.set.created` | Set created |
| `mds.set.deleted` | Set deleted |
| `mds.container.created` | Container created |
| `mds.container.renamed` | Container renamed or moved |
| `mds.container.deleted` | Container deleted |
| `mds.item.added` | Item added |
| `mds.item.flagged` | Item flags changed |
| `mds.item.copied` | Membership copied |
| `mds.item.moved` | Membership moved |
| `mds.item.removed` | Membership removed |
| `mds.gc.completed` | Garbage collection completed |
| `mds.verify.completed` | Verification completed |
| `mds.verify.failed` | One blob failed verification |
| `mds.changelog.pruned` | A retained change stream was pruned |

With the `cosmix` feature, `spawn_publisher_task` drains an `MdsEvent` receiver and sends non-retained `topic.publish` requests through `NodedClient`. `SqliteCasMds::with_bus_events` installs a default-capacity event sink and returns its first receiver.

The crate exposes events, not a Bus command or verb service.

## Maintenance

`verify_blobs` supports full verification, verification since a time, or verification of blobs referenced by a container. It recomputes BLAKE3 and records success, hash mismatch, or missing-file status in the verification ledger.

`gc` performs two passes over unreferenced blobs with a quiescence interval between passes. It rechecks refcounts and last-seen timestamps before deletion. Dry-run mode reports the work without unlinking files or deleting rows.

`rebuild_index` reconstructs the shared blob index from every per-set database and the CAS. It preserves the verification ledger and reports valid CAS files that are not represented in the rebuilt index.

`export_set` writes a tar archive containing `manifest.json`, a WAL-free `data.sqlite` snapshot, and each distinct referenced CAS blob. It does not export `blobs.sqlite`.

`import_set` validates the archive layout, manifest, schema version, entry paths, and every blob hash before installing the set. It refuses an existing set UUID and reconstructs the imported set's blob references.

See [CLI reference](cli.md) for the operator commands.

## Configuration

| Setting | Meaning |
|---|---|
| `COSMIX_MDS_ROOT` | Storage root used by the CLI when `--root` is absent |
| `COSMIX_MDS_GC_QUIESCENCE_SECS` | GC pass interval in integer seconds |

The GC interval defaults to 60 seconds. Values below 5 seconds in `COSMIX_MDS_GC_QUIESCENCE_SECS` are ignored and the default remains in effect. `with_gc_quiescence` bypasses that floor for tests.

## Cargo features

| Feature | Default | Effect |
|---|---:|---|
| `core` | Yes | Default storage profile; this marker adds no dependencies |
| `cosmix` | No | Adds `cosmix-lib-bus` and native `cosmix-lib-client`, and exposes `spawn_publisher_task` |
| `mem-store` | No | Exposes `MemMds`; its current `Mds` methods are stubs that panic with `unimplemented!()` |
| `_stress-helper` | No | Internal only; builds `cosmix-mds-stress-writer` for crash-recovery tests |

## Modules

| Module | Purpose |
|---|---|
| `store` | `Mds`, `SqliteCasMds`, per-set locking, transactions, and maintenance orchestration |
| `types` | Identifiers, flags, tags, records, events, scopes, and reports |
| `container` | Container, item, membership, search, and changelog SQL operations |
| `blob` | BLAKE3 hashing and atomic CAS file operations |
| `blob_index` | Shared blob index, references, refcounts, and verification ledger |
| `schema` | Versioned SQLite migration runners |
| `notifier` | Per-container in-process broadcast channels |
| `bus` | Typed event taxonomy, event sink, and optional Bus publisher |
| `verify` | Blob integrity verification |
| `rebuild` | Derived blob-index reconstruction |
| `export`, `import` | Set archive transfer |
| `gc` | Garbage-collection module boundary |
| `error` | Typed crate errors |
| `mem` | Feature-gated in-memory stub |

## Dependencies

SQLite persistence uses `rusqlite`; hashing uses `blake3`; identifiers use UUID v7 support. Tokio provides broadcast channels, `parking_lot` provides locks, and Serde supplies typed JSON payloads. The operator binary uses Clap and `humantime`; archive transfer uses `tar` and `chrono`. Bus dependencies remain optional.
