# Media blob references

CMS images remain served from their existing `img/YYYY/MM/<hash-prefix>.<ext>`
files. Upload writes the file atomically and commits the media row first, then
spawns an optional local blob write with admission acquired before spawning.
The HTTP redirect never waits for the lane. A blob outage, quota refusal or saturated
pool leaves the upload successful with `blob=NULL`; the existing 303 redirect
to `/admin/media` is unchanged. No file is replaced by a blob URL this arc.

`webd.media.ref {vhost, id}` returns the complete reference object directly:
`{blob, size, mime, name?, origin}`. `id` is a positive JSON integer and `vhost`
is a configured primary or alias. The verb is mesh-open and mutating: a missing
reference retries from the served disk file. It runs on the same eight
session-owned Bus workers as share management, with an additional eight-operation
media-reference admission bound. Concurrent retries for one primary-vhost/id
coalesce; after the first attaches, followers return the stored reference.

Retries refuse non-disk rows with `unsupported_storage:`. Files are opened beneath
the document-root descriptor, checked as regular files, bounded to 10 MiB, and
checked against the media row's byte count and 32-hex filename hash. Blob identity
uses the **full** BLAKE3 hash. The adapter checks local `blob.stat` first, recovers
or establishes the owner pin, and uploads missing bytes with hash-addressed PUT.
The owner is `webd:media:<first-16-hex-of-BLAKE3(primary-fqdn)>`. Stat recovery
omits optional `name`, which that verb does not provide.

No database lock spans filesystem reads or lane calls. Before attaching the
reference, webd rechecks the row: deletion returns `not_found`; a changed row
returns `conflict:`. The blob pin remains in either case. Delete also retains
pins; release belongs to later reconciliation.

Both native `ensure_media_schema` and Mix `cms_init` add `blob TEXT NULL` while
preserving legacy rows. Mix catches only duplicate-column ALTER errors through
the existing DB authoriser, without PRAGMA access; other schema
failures propagate. The column holds validated reference JSON, not a hash-only
string or a separate origin column.

Atomic writes use unique temporary files in the destination directory. Upload,
rollback, reference reads/attachment and deletion coordinate by physical path,
including aliases of one document root. Deletion checks references in active
vhost catalogues by device/inode before unlinking, including symlinked image
directories and hard-linked files; ambiguous database or filesystem state retains
the file.
Eight write operations are admitted. A cancelled blocking file write keeps its
path lock and admission until it finishes. Process death between file publication
and row insertion can still leave an orphan file for later reconciliation.

Bus success is rc=0; failures are rc=10 with `{error: "token: details"}`. Errors
include `invalid_arguments:`, `not_found`, `unsupported_storage:`, `too_large:`,
`conflict:`, `busy:`, `internal:` and the lane taxonomy (`lane_unavailable:`,
`quota:`, `lane:`, `not_present:`, `verify_failed:`). A valid stored reference is
returned without a lane round trip; pin reconciliation is not performed here.
