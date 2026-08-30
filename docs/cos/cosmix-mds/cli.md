# cosmix-mds command

`cosmix-mds` is the operator interface to a `SqliteCasMds` storage root. It lists and migrates sets, reports statistics, verifies and collects blobs, rebuilds the derived blob index, transfers sets, and prunes retained change streams.

## Synopsis

```text
cosmix-mds [OPTIONS] <COMMAND>
```

The storage root is mandatory. Pass `--root <PATH>` or set `COSMIX_MDS_ROOT`. The command-line option takes precedence over the environment variable. There is no implicit root.

## Global options

| Option | Meaning |
|---|---|
| `--root <PATH>` | MDS storage root |
| `--json` | Emit the command report as JSON |
| `-q`, `--quiet` | Accepted global flag; current handlers do not consult it |
| `-v`, `--verbose` | Accepted global flag; current handlers do not consult it |
| `--help` | Show help |
| `--version` | Show the package version |

Reports go to standard output. Command failures print `cosmix-mds: <error>` to standard error.
## Exit status

| Status | Meaning |
|---:|---|
| `0` | Command succeeded, including a report containing findings |
| `2` | Argument, root, I/O, schema, UUID, duration, or storage failure |

Status `1` is reserved and is not emitted by the current implementation. Verification mismatches, garbage-collection deletions, and dry-run pending work do not change a successful status to a failure.
## Commands

| Command | Purpose |
|---|---|
| `list-sets` | Print every set UUID |
| `migrate-all` | Open the root and apply all known schema migrations |
| `stats [--per-set]` | Report global counters and optional per-set counters |
| `verify [SCOPE]` | Recompute blob hashes and update the verification ledger |
| `gc [--dry-run]` | Run two-pass garbage collection |
| `rebuild-index` | Reconstruct `blobs.sqlite` |
| `export <SET_ID> <TARBALL>` | Export one set |
| `import <TARBALL>` | Import one exported set |
| `prune-changelog <SET_ID> --stream <STREAM> --keep-n <N>` | Retain the newest rows in a set change stream |

## list-sets

```text
cosmix-mds --root <PATH> list-sets
```

Human output contains one UUID per line. JSON output has this shape:

```json
{"sets":["018f0000-0000-7000-8000-000000000001"]}
```

Opening the root applies known migrations before listing sets.
## migrate-all

```text
cosmix-mds --root <PATH> migrate-all
```

The command opens the shared blob database and every discovered set database. Opening applies all known migrations. The current implementation stops at the first error; it does not continue with later sets.

Successful JSON output contains `sets` and `errors`. `errors` is currently always zero because partial success is not implemented.

```json
{"sets":3,"errors":0}
```

## stats

```text
cosmix-mds --root <PATH> stats
cosmix-mds --root <PATH> stats --per-set
```

Global statistics contain:

- Set count.
- Container count.
- Item count.
- Physical distinct blob count.
- Total physical blob bytes.
- Deduplication ratio.

`--per-set` appends a row for each set in human output. In JSON, the stable top-level `sets` field is `null` without `--per-set` and an array when the option is present.

Per-set `blob_count` counts distinct hashes referenced by that set. Per-set `total_bytes` is logical item size, not deduplicated physical storage.

## verify

```text
cosmix-mds --root <PATH> verify
cosmix-mds --root <PATH> verify --full
cosmix-mds --root <PATH> verify --since 24h
cosmix-mds --root <PATH> verify --container <CONTAINER_UUID>
```

The scope options are mutually exclusive:

| Scope | Selection |
|---|---|
| No scope or `--full` | Every blob row in `blobs.sqlite` |
| `--since <DURATION>` | Blobs never verified or last verified before the calculated time |
| `--container <UUID>` | Blobs referenced by that container across all sets |

Durations use `humantime` syntax such as `30m`, `24h`, `7d`, or `1h30m`.

The report separates hash mismatches from missing CAS files. JSON fields are `blobs_checked`, `mismatches`, `mismatches_hash`, `mismatches_missing`, `duration_ms`, and `scope`.

## gc

```text
cosmix-mds --root <PATH> gc
cosmix-mds --root <PATH> gc --dry-run
```

Garbage collection takes a first snapshot of zero-refcount candidates, waits for the configured quiescence interval, then rechecks each candidate before deleting it. A blob is skipped if it was referenced again or touched during the interval.

`--dry-run` performs both passes and rechecks but does not unlink CAS files or delete index rows. Its counters describe what would be removed.

The report includes deleted blobs, bytes freed, first-pass candidates, re-referenced and re-touched skips, orphan rows swept, pending rows observed, and elapsed milliseconds. JSON also carries `dry_run`.

The interval defaults to 60 seconds. `COSMIX_MDS_GC_QUIESCENCE_SECS` overrides it with an integer number of seconds; values below 5 are ignored.

## rebuild-index

```text
cosmix-mds --root <PATH> rebuild-index
```

`rebuild-index` recreates the shared blob and reference rows from all per-set `data.sqlite` files. It preserves the verification ledger.

The report contains sets scanned, items indexed, blobs indexed, orphan CAS files found, and elapsed milliseconds. An orphan is a valid hash-named CAS file absent from the rebuilt index.

## export

```text
cosmix-mds --root <PATH> export <SET_UUID> <TARBALL_PATH>
```

The command locks the set for the export, creates a compact `data.sqlite` snapshot, gathers each distinct referenced blob, and writes:

```text
manifest.json
data.sqlite
blobs/<hh>/<hh>/<blake3-hex>
```

The archive does not contain `blobs.sqlite`. The destination is written through a sibling `.partial` file and renamed after completion. A missing referenced blob or write failure aborts the export and removes the partial output where possible.

JSON fields are `set_id`, `tarball`, `item_count`, `blob_count`, `bytes_written`, and `duration_ms`.

## import

```text
cosmix-mds --root <PATH> import <TARBALL_PATH>
```

The set UUID comes from `manifest.json`; it is not a command argument. Import refuses a UUID already present in the root.

Import accepts only the fixed export layout. It rejects absolute paths, traversal paths, duplicate required entries, unsupported format or schema versions, inconsistent counts, and blob paths whose BLAKE3 value does not match their bytes.

The command stages data before making the set visible, migrates older supported set schemas, installs the set database, and writes the imported blob references. It does not populate the later verification ledger.

JSON fields are `set_id`, `tarball`, `item_count`, `blob_count`, `bytes_read`, and `duration_ms`.

## prune-changelog

```text
cosmix-mds --root <PATH> prune-changelog <SET_UUID> \
  --stream container-change-set --keep-n 1000

cosmix-mds --root <PATH> prune-changelog <SET_UUID> \
  --stream set-change --keep-n 1000
```

The supported streams are:

| CLI value | Stored stream |
|---|---|
| `container-change-set` | Set-wide container lifecycle changes |
| `set-change` | Set-wide item changes |

The command keeps the newest `N` rows and advances the durable retention floor to the highest deleted sequence. `--keep-n 0` removes every row in the selected stream. If fewer than `N` rows exist, the command is a no-op and leaves the floor unchanged.

JSON fields are `set_id`, `stream`, `keep_n`, `rows_removed`, and `new_floor`.

## JSON conventions

Each successful JSON report is one object followed by a newline. Durations use integer `duration_ms` fields. UUIDs and paths are strings. Human-readable byte values use decimal units, while JSON byte counters remain integers.

Use `--json` for automation. Human output is intended for direct inspection and may contain aligned columns, thousands separators, and abbreviated durations.
## See also

[cosmix-mds library](README.md)
