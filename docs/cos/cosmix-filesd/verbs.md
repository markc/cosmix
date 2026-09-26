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
| `fs.blob.ref` | Required `path`; optional `name` (source filename), `mime` (source extension) |
| `fs.blob.materialise` | Required `blob`, `path`; optional `overwrite` (`false`) |
| `fs.copy` | Required `from` and `to`; optional `overwrite` (`false`) |
| `fs.move` | Required `from` and `to`; optional `overwrite` (`false`) |
| `fs.trash` | Required `path` |
| `fs.trash.list` | None |
| `fs.trash.restore` | Required `token` |

Write operations fail for a read-only place.

### Binary blob bridge

Both blob verbs have `read_only=false` and have no unprefixed aliases.
Filesystem dispatch runs concurrently with 64 permits; saturation waits for a
permit. Lane discovery times out after 10 seconds. Transfers remain synchronous:
size `timeout=` on Mix `send` for the file. Cross-node calls are capped by the
30-second mesh response budget, so drive large transfers from the node owning
the place. A transfer can finish and pin/land after the caller times out;
retrying materialise then answers `exists:` if the earlier transfer landed.
`fs.blob.ref` changes blobd's pin state but only needs **read** access to its
source place; a read-only place is a valid source. `fs.blob.materialise` needs
a writable destination place. The existing delegation gate applies to both.

`fs.blob.ref {path, name?, mime?}` opens a plain, single-link regular file under
the place jail and streams it to `POST /blob` on the configured `blob_service`'s
byte lane. Place roots, directories, symlink files, hardlinks, special files and
policy-prefix paths are refused. The body uses an explicit `Content-Length`;
the pin owner is the filesd `bus_service`. The MIME default is the same extension
table as `fs.read_blob`. A 201 response returns the validated blob reference
with `path` added; optional reference attributes (including a named store's
`instance`) are preserved. File bytes are never carried in a Bus frame.

Filename hints (including an explicit `name`) percent-encode UTF-8 bytes outside
printable non-space ASCII, plus `%`, for the HTTP header. blobd currently stores
the encoded name verbatim, so `Résumé.pdf` returns as `R%C3%A9sum%C3%A9.pdf`.
An encoded name longer than 128 bytes is omitted; the reference name is null.

`fs.blob.materialise {blob, path, overwrite?}` accepts `b3:<64 lowercase hex>`,
bare lowercase hex, or a reference map containing `blob`. It streams
`GET /blob/<hex>` from that local store. A missing blob must first be fetched
with `blob.fetch`; materialise does not initiate a cross-node fetch.

The destination uses the write jail and rejects existing non-plain targets.
Missing parent directories are created, matching `fs.write`. Bytes stream into
a unique sibling temporary file, with length and BLAKE3 verification before
publication. An overwrite preserves mode bits and uses fsync then rename;
`overwrite=false` uses Linux `renameat2(RENAME_NOREPLACE)` to publish in one
step and refuse a destination created during the download. Unsupported
rename flags fall back to link/unlink (a crash between those steps can leave
two names). Where links are unsupported, the last tier is best-effort
check-then-rename and cannot exclude a concurrent target creation. New files
are mode 0600 on Unix. Ownership is not
preserved. Staging is removed on errors; the old target survives failed reads
or verification. Directory fsync is best-effort after publication.

Success is `{"ok":true,"path":...,"blob":"b3:...","size":N}`. The HTTP
client has 30-second connect/read/write bounds, refuses redirects and encoded
responses, and requires `Content-Length` on GET. The reader-driven landing
primitive checks short and long inputs. ureq 2 exposes only the declared HTTP
body, so trailing wire bytes beyond `Content-Length` are not visible to it;
the framed body must still match the requested hash. The existing resolver's
documented path-based check/use race posture is unchanged.

All failures use rc 10 and `{"error":...}`:

| Leading text | Meaning |
|---|---|
| `lane_unavailable:` | Lane props failed, timed out, or supplied an empty/invalid bind |
| `quota:` | HTTP 413, including a bounded prefix of blobd's error body |
| `lane:` | Other HTTP/transport errors, invalid references or response framing; a mid-upload close includes `lane closed during upload` and a quota hint |
| `not_present:` | `blob is not on this node — blob.fetch it first` (GET 404) |
| `verify_failed:` | Body length or BLAKE3 mismatch |
| `invalid blob id` | Invalid materialise `blob` argument |
| `denied:`, `not found:`, `exists:`, `bad request:`, `i/o error:` | Filesystem-layer errors, passed through unchanged |

A truncated HTTP body reported by ureq as a read error uses `lane:`. A short
reader ending cleanly uses `verify_failed:`. A failed call may have created
parent directories; a failed upload response does not undo a pin already
committed by blobd.

### Irreversible operations

| Verb | Arguments |
|---|---|
| `fs.delete` | Required `path` and `confirm: true`; optional `recursive` (`false`) |
| `fs.trash.empty` | Required `confirm: true` |

Calls without the exact boolean `confirm: true` fail with result code `10`.

## Authorisation boundary

An envelope-bearing call is accepted only from a peer named in `delegated_peers` and only when the envelope validates. An allowlisted delegated peer cannot use the bare command path.

The current delegated envelope authorises the complete selected namespace for an administrator. There is no per-actor or per-corpus grant check in this crate.
