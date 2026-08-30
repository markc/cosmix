# Bus verbs

`cosmix-filesd` exposes one Bus namespace per operating mode. Corpus mode uses `filesd.*`; filesystem mode uses `fs.*`.

## Calling convention

For a bare command, arguments are resolved in this order:

1. JSON from the Bus `args` header.
2. The command's non-null `args` value.
3. JSON parsed from the raw body.

A delegated command instead carries a top-level `$cosmix_delegation` object and a separate `args` object. See [README.md](README.md#delegated-calls).

Success uses result code `0`. Errors use result code `10` and a body shaped as:

```json
{"error":"message"}
```

## Corpus verbs

### `filesd.list`

List live indexed documents.

| Argument | Default | Constraints |
|---|---:|---|
| `limit` | `200` | Clamped to 1 through 1000 |
| `offset` | `0` | Negative values become 0 |

The response contains `rows` and the live document `total`.

### `filesd.read`

Read indexed metadata and, when permitted by size, the current file body.

| Argument | Requirement |
|---|---|
| `id` | Document ID; takes precedence when both selectors are present |
| `path` | Corpus-relative path, used when `id` is absent |

One selector is required. The response contains `doc`. Bodies larger than 4 MiB are omitted and `body_truncated` is set to `true`.

### `filesd.search`

Search indexed documents.

| Argument | Default | Constraints |
|---|---:|---|
| `q` or `query` | Required | Must be a non-empty string |
| `limit` | `50` | Clamped to 1 through 1000 |

The response contains matching `rows` and `count`.

### `filesd.changes`

Read the corpus change stream.

| Argument | Default | Constraints |
|---|---:|---|
| `since` | `0` | Accepts a decimal string or JSON integer |
| `limit` | `200` | Clamped to 1 through 1000 |

The response contains `changes` and `next`. Each change contains string `modseq`, `doc_id`, `kind`, and `changed_at`. `next` is the last returned sequence as a string, or null when no change is returned.

### `filesd.save`

Atomically write and index a document.

| Argument | Default | Constraints |
|---|---:|---|
| `path` | Required | Safe, non-empty corpus-relative path |
| `content` | Empty string | String |

The daemon preserves an existing ID when ID-less content replaces an indexed file. Otherwise it ensures the content has an ID, creating one when needed.

The response contains `ok`, `id`, `path`, and string `modseq`.

### `filesd.move`

Rename a file and its index entry.

| Argument | Requirement |
|---|---|
| `from` | Existing safe corpus-relative source |
| `to` | Safe corpus-relative destination that does not exist |

Missing source index state is recovered by ingesting the moved file as a new index entry. The response contains `ok`, `from`, `to`, and string `modseq`.

### `filesd.delete`

Remove a file and tombstone its index entry.

| Argument | Requirement |
|---|---|
| `path` | Safe corpus-relative path |

A missing file is accepted, making the filesystem removal idempotent. The response contains `ok`, `path`, and string `modseq`; `modseq` is null when no indexed row exists.

### `filesd.resync`

Republish every live document to the semantic index and issue purges for tombstoned paths. The response is:

```json
{"ok":true}
```

### `filesd.props.get`

Read the complete property snapshot or a selected property path.

### `filesd.props.list`

List the property paths exposed by the daemon.

### `filesd.props.describe`

Describe a property path, including its type and metadata.

The property surface is read-only L1. Available paths are listed in [README.md](README.md#properties).

## Filesystem verbs

Filesystem paths begin with a configured place ID. Place writability and allow or deny rules apply before an operation reaches the underlying path.

### Read operations

| Verb | Arguments |
|---|---|
| `fs.places` | None |
| `fs.list` | Required `path`; optional `show_hidden` (`false`), `sort` (`name`), `dir` (`asc`) |
| `fs.stat` | Required `path` |
| `fs.tree` | Required `path`; optional `max_depth` (`4`, clamped 1-8), `max_nodes` (`1000`, clamped 1-5000) |
| `fs.read_blob` | Required `path`; optional byte `max` (1 MiB) |
| `fs.search` | Required `path` and `query` or `q`; optional `recursive` (`true`) and `limit` (`200`) |

`fs.places` returns the configured place descriptions. Other response shapes come directly from the filesystem layer.

### Reversible write operations

| Verb | Arguments |
|---|---|
| `fs.mkdir` | Required `path`; optional `parents` (`false`) |
| `fs.touch` | Required `path` |
| `fs.write` | Required `path`; optional `content` (empty string), `overwrite` (`false`) |
| `fs.copy` | Required `from` and `to`; optional `overwrite` (`false`) |
| `fs.move` | Required `from` and `to`; optional `overwrite` (`false`) |
| `fs.trash` | Required `path` |
| `fs.trash.list` | None |
| `fs.trash.restore` | Required `token` |

Write operations fail for a read-only place.

### Irreversible operations

| Verb | Arguments |
|---|---|
| `fs.delete` | Required `path` and `confirm: true`; optional `recursive` (`false`) |
| `fs.trash.empty` | Required `confirm: true` |

Calls without the exact boolean `confirm: true` fail with result code `10`.

## Authorisation boundary

An envelope-bearing call is accepted only from a peer named in `delegated_peers` and only when the envelope validates. An allowlisted delegated peer cannot use the bare command path.

The current delegated envelope authorises the complete selected namespace for an administrator. There is no per-actor or per-corpus grant check in this crate.
