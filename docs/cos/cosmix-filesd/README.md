# cosmix-filesd

`cosmix-filesd` serves files as authoritative data. In corpus mode it watches a Markdown tree, maintains a rebuildable SQLite index, reconciles external changes, and exposes document operations over Bus. In `fs` mode it exposes a configured set of filesystem places without a corpus index. It belongs to the `cos` daemon layer in the `bus <- mix <- cos` dependency chain and consumes Bus and Cos substrate crates.

## Synopsis

```text
cosmix-filesd reconcile [OPTIONS]
cosmix-filesd serve [OPTIONS]
```

The package builds the `cosmix-filesd` binary. It does not expose a Rust library target.

## Operating modes

| Mode | Purpose | Runtime state | Bus namespace |
|---|---|---|---|
| Corpus | Manage and index one Markdown corpus | Filesystem watcher, periodic reconcile, SQLite store, index writer | `filesd.*` |
| Filesystem | Serve an allowlisted set of file-manager places | Stateless filesystem layer | `fs.*` |

`serve` selects filesystem mode only when its configuration contains `mode: fs`. All other invocations use corpus mode.

## Commands

### `reconcile`

Walk the corpus once, update its index, print a summary to standard error, and exit.

```text
cosmix-filesd reconcile \
  --root /var/lib/cosmix/corpora/notes \
  --corpus notes \
  --db /var/lib/cosmix/filesd/notes.db
```

Options:

| Option | Meaning |
|---|---|
| `-c`, `--config PATH` | Read a flat `key: value` configuration file |
| `--root PATH` | Override the corpus root |
| `--corpus ID` | Override the corpus identifier |
| `--db PATH` | Override the SQLite index path |

`reconcile` supports corpus configuration only.

### `serve`

Run the selected service mode and register a Bus service.

```text
cosmix-filesd serve -c /etc/cosmix/filesd/notes.conf.mix
```

Options:

| Option | Meaning |
|---|---|
| `-c`, `--config PATH` | Read the service configuration |
| `--root PATH` | Override the corpus root |
| `--corpus ID` | Override the corpus identifier |
| `--db PATH` | Override the SQLite index path |
| `--interval-secs N` | Override the periodic reconcile interval |
| `--bus-service NAME` | Override the registered Bus service name |

The corpus flags apply to corpus mode. Filesystem mode requires a configuration file because repeated `place:` entries are read from the original text.

See [configuration.md](configuration.md) for both configuration schemas.

## Corpus service

Corpus mode performs a reconcile at startup. A recursive filesystem watcher queues another reconcile after relevant event bursts, and a periodic timer provides a correctness backstop.

One dedicated writer thread owns the SQLite store. Watch events, timer ticks, and Bus commands send work to that thread, so index mutations are serial.

The watcher considers non-hidden Markdown paths and whole-file conflict paths. Its debounce window is 250 milliseconds.

The SQLite index is derived state. Files on disk remain authoritative.

`filesd.save` is the only operation that mints a missing document ID. It writes atomically, scans the resulting file, and updates the index before replying. Saving ID-less content over an existing document preserves that document's ID.

Corpus-relative paths reject absolute paths, empty paths, parent traversal, and symlink escapes. `filesd.move` also refuses to replace an existing destination.

Corpus changes are pushed to `indexd.index_file` while the Bus broker is connected. Reconnection replays pending changes and republishes the live corpus. `filesd.resync` requests a full live republish and tombstone purge.

## Filesystem service

Filesystem mode serves configured places through a stateless filesystem layer. It has no SQLite store, watcher, reconcile timer, or index push.

Each place has an identifier, display metadata, a root, a writable flag, and optional allow and deny patterns. Bus operations run as blocking filesystem work outside the asynchronous command loop.

Irreversible operations require `confirm: true`. Moving a file to trash is the reversible default.

## Bus interface

Corpus mode registers these commands:

- `filesd.list`
- `filesd.read`
- `filesd.search`
- `filesd.changes`
- `filesd.save`
- `filesd.move`
- `filesd.delete`
- `filesd.resync`
- `filesd.props.get`
- `filesd.props.list`
- `filesd.props.describe`

Filesystem mode registers these commands:

- `fs.places`
- `fs.list`
- `fs.stat`
- `fs.tree`
- `fs.read_blob`
- `fs.search`
- `fs.mkdir`
- `fs.touch`
- `fs.write`
- `fs.copy`
- `fs.move`
- `fs.trash`
- `fs.trash.list`
- `fs.trash.restore`
- `fs.trash.empty`
- `fs.delete`

Successful commands return Bus result code `0` with a JSON body. Handler, validation, authorisation, and filesystem errors return code `10` with an `error` field.

See [verbs.md](verbs.md) for arguments, defaults, limits, and response shapes.

## Properties

Corpus mode provides a read-only SPEC-07 L1 property tree.

| Path | Type | Meaning |
|---|---|---|
| `config.corpus_id` | String | Corpus identifier |
| `config.root` | String | Watched corpus root |
| `lifecycle.started_at` | String | Process start time in RFC 3339 form |
| `lifecycle.uptime_s` | Number | Process uptime in seconds |
| `lifecycle.props_level` | String | Conformance level, always `L1` |
| `corpus.files` | Number | Live, non-tombstoned documents |
| `corpus.conflicts` | Number | Whole-file conflicts observed |
| `corpus.unmanaged` | Number | Live files without managed IDs |
| `corpus.bytes` | Number | Total size of live documents |
| `corpus.modseq` | String | Current corpus change sequence |

The property surface supports `get`, `list`, and `describe`. It does not publish property change events, watches, or retained `world.*` state.

Document IDs, change sequence values, and change cursors cross Bus as strings to avoid JSON number precision loss.

## Delegated calls

Both modes share the same delegated-call gate. A top-level `$cosmix_delegation` field selects the delegated path. Its presence is decisive: an invalid envelope is rejected and never falls back to the bare path.

Delegated calls must come from a configured peer and carry envelope version `1`, role `admin`, boolean `csrf_verified: true`, and non-empty `actor`, `vhost`, `route_id`, and `request_id` fields. The command arguments are read from the envelope's separate `args` object.

A configured delegated peer must use an envelope. Other peers use the bare trusted path. Delegated outcomes and security refusals are written to standard error for service logging.

## Build

`cosmix-filesd` depends on the Cos file, configuration, and property substrates, plus Bus client, protocol, and build-information crates. Its SQLite support is enabled through `cosmix-lib-files`.

The crate defines no Cargo feature flags.
