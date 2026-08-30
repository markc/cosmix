# Configuration

`cosmix-filesd` reads a flat configuration format with one `key: value` pair per line. Blank lines and lines beginning with `#` are ignored. Keys and values are trimmed.

The conventional suffix is `.conf.mix`, but the parser handles the flat syntax directly.

CLI options override corresponding corpus configuration values. `--bus-service` also overrides the filesystem-mode service name.

## Corpus mode

Corpus mode is the default. It requires `root`, `corpus_id`, and `db`.

```text
root: /var/lib/cosmix/corpora/notes
corpus_id: notes
db: /var/lib/cosmix/filesd/notes.db
interval_secs: 900
bus_service: filesd-notes
delegated_peers: webd
```

| Key | Required | Default | Meaning |
|---|---|---|---|
| `mode` | No | `corpus` | Service mode; any value other than `fs` selects corpus mode |
| `root` | Yes | None | Markdown corpus root |
| `corpus_id` | Yes | None | Corpus identifier and semantic-index domain |
| `db` | Yes | None | SQLite index path |
| `interval_secs` | No | `900` | Periodic reconcile interval in seconds |
| `bus_service` | No | `filesd-<corpus_id>` | Bus service registration name |
| `delegated_peers` | No | `webd` | Comma-separated peers allowed to send delegated calls |

An explicitly empty `delegated_peers:` disables delegated calls. An absent key enables the default peer.

The database parent directory is created when required. Each daemon instance serves one corpus and owns one store.

## Filesystem mode

Filesystem mode requires `mode: fs`, `trash_root`, and at least one `place:` line.

```text
mode: fs
bus_service: filesd-fs
trash_root: /var/lib/cosmix/filesd/trash
delegated_peers: webd

place: documents | Documents | /home/example/Documents | group=places | icon=folder | writable=true
place: public | Public | /var/lib/cosmix/public | writable=false | allow=*.md,manuals/*
```

| Key | Required | Default | Meaning |
|---|---|---|---|
| `mode` | Yes | None | Must be `fs` |
| `bus_service` | No | `filesd-fs` | Bus service registration name |
| `trash_root` | Yes | None | Root used by trash operations |
| `delegated_peers` | No | `webd` | Comma-separated peers allowed to send delegated calls |
| `place` | One or more | None | Place definition; repeated lines are allowed |

An explicitly empty `delegated_peers:` disables delegated calls in this mode as well.

## Place definitions

The place form is:

```text
place: ID | LABEL | ROOT | group=GROUP | icon=ICON | writable=BOOL | allow=PATTERNS | deny=PATTERNS
```

The first three fields are positional:

| Field | Requirement |
|---|---|
| `ID` | Non-empty and contains no `/` |
| `LABEL` | Display label; may be empty |
| `ROOT` | Non-empty filesystem root |

Options may follow in any order:

| Option | Default | Meaning |
|---|---|---|
| `group` | `places` | Display grouping |
| `icon` | `folder` | Display icon name |
| `writable` | `false` | Enables write verbs when `true`, `1`, or `yes` |
| `allow` | Empty | Comma-separated relative path patterns to include |
| `deny` | Empty | Comma-separated relative path patterns to exclude |

Unknown options and duplicate options are errors.

## Path patterns

`allow` and `deny` values contain comma-separated place-relative glob patterns.

Accepted pattern characters are:

```text
A-Z a-z 0-9 . _ * - /
```

A pattern must not:

- begin with `/`;
- contain an empty, `.` or `..` path segment;
- contain whitespace or other punctuation.

If `allow=` or `deny=` appears, it must contain at least one pattern. An empty explicit policy is rejected because it could otherwise remove a restriction.

Omitting both options leaves the place without an allow or deny pattern policy.

## Delegation settings

`delegated_peers` is a comma-separated list. Whitespace is trimmed and empty list items are discarded.

Each listed peer must send a `$cosmix_delegation` envelope. A listed peer using the bare path is rejected. A peer not on the list cannot use the delegated path.

The envelope format and command behaviour are described in [verbs.md](verbs.md#authorisation-boundary).

## CLI precedence

For corpus mode, command-line values take precedence over the file:

```text
cosmix-filesd serve \
  -c /etc/cosmix/filesd/notes.conf.mix \
  --interval-secs 300 \
  --bus-service filesd-notes-preview
```

Values not supplied on the command line remain sourced from the file or their defaults.

Filesystem mode reads repeated `place:` lines from the raw file text. It accepts only `--bus-service` as a mode-specific override; corpus path and index flags do not configure its places.
