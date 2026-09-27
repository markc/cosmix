# Public shares

Public file shares are available through explicit revocable tokens. Catalogue
management is available over the Bus and authenticated HTTP.

Compatibility: `/s/…` and `/api/shares*` are reserved on every vhost, ahead of
static files and Mix handlers. Existing applications must move conflicting routes.

Account roots must not be equal to or beneath any vhost's canonical `www_dir`
or `docs_dir`, including symlink aliases. Startup skips and logs conflicting
roots; access rechecks the current routing snapshot after reload. These checks
do not make an already public directory private: move private bytes out of it.
The service's `ProtectHome=yes` makes roots under `/home` unavailable. Provision
roots in a service-accessible directory. The host is assumed to enforce
`fs.protected_hardlinks`; descriptor confinement cannot distinguish hard links.

Rows carry `primary_fqdn`; resolve, list and revoke require that primary.
Duplicate canonical CMS database paths disable the later primary at startup.
Rows predating this ownership column remain intact with NULL ownership and
cannot be served until explicitly mapped to their correct primary.

## Public downloads

`GET/HEAD /s/{token}` is a per-vhost route ahead of static serving. Unknown,
revoked, expired, unmapped legacy or unsupported-kind tokens terminate with 404;
they never fall through to a similarly named static file. Other methods return
405 with `Allow: GET, HEAD`. A missing trusted connection context returns 503.

Password-protected shares require HTTPS Basic authentication: any username,
password in the password part, `WWW-Authenticate: Basic realm="share"` on 401.
Plain requests reaching this handler return 403 without a challenge; the existing
redirect-only HTTP listener still redirects to HTTPS. No query-string password is read.
Before bcrypt, five failed verifications per (token, socket peer IPv4 or IPv6 /64)
per 60 seconds are allowed. Missing credentials and successful checks do not count.
The table holds at most 4096 pairs; insertion evicts the oldest entry for that
token, or the oldest global entry when the token has none. Refusal returns 429
with `Retry-After: 60`. Forwarded IP headers are ignored. Creation has a separate
one-slot bcrypt worker; verification has four workers.
The catalogue gate is reloaded after password verification and before target access.

Successful reads return 200 or single-range 206; unsatisfiable ranges return
416 with `Content-Range: bytes */<size>` and an empty body. HEAD mirrors GET's
headers without body bytes. All token-route responses carry:

```http
Cache-Control: private, no-store
X-Content-Type-Options: nosniff
Content-Security-Policy: sandbox
```

File responses also carry `Accept-Ranges: bytes`, exact `Content-Length`, and
`Content-Disposition: attachment; filename*=UTF-8''<percent-encoded-name>`.
Paths use `application/octet-stream`; blobs use the validated reference MIME.
With `If-Range` present, Range is ignored and the full representation is returned:
there is no ETag or Last-Modified validation contract for shares.
Blob headers are allowlisted; no upstream cache, cookies or redirects propagate.
At most eight public bodies are admitted, with no waiting queue (503 `busy:`).
File reads use bounded 64 KiB buffers. A timer-driven body pump aborts after
30 seconds without downstream progress or one hour total, releasing both public
and lane admission even when the client stops reading. It buffers one chunk.
The best-effort counter increments once as the first non-empty body chunk emits,
with a five-second telemetry deadline; HEAD, 416 and empty files do not count.

## Management

Root keys may be `"primary.example|user@example.test"` to bind an identity to
one primary vhost. Scoped keys take precedence over email-only keys. Startup
warns when unscoped roots coexist with different `jmap_upstream` providers;
use scoped keys in that configuration.

Mesh-open Bus verbs use JSON arguments (no capability check):

- `webd.share.create {vhost, account, rel_path?, blob?, kind?, name?, password?, expires?}`
  requires exactly one target; `kind` defaults to `file`. `blob` is the complete
  reference object, not an ID string. `name` overrides its optional name and is
  refused for path targets. Returns `{token, url}` (relative `/s/...` URL).
- `webd.share.list {vhost, account, after?, limit?}` returns `{shares, next, skipped}`.
  Unservable rows are skipped and counted; `next` advances past all scanned rows,
  including skipped ones. The HTTP list returns the same shape.
  Limit defaults to 100 (range 1..100); `next` is the last token or null.
- `webd.share.revoke {vhost, account, token}` returns `{revoked: bool}`; false
  includes an absent, already revoked or differently owned token.

`vhost` accepts a configured primary or alias; catalogues and blob owners use
the primary. Success is Bus rc=0; errors are rc=10 with `{error: "token..."}`.
Eight session-owned workers perform these operations; broker disconnect aborts
them. Saturation returns `busy: webd transfer workers full (8)`.

HTTP JSON management is `GET/POST /api/shares` and
`POST /api/shares/{token}/revoke`. GET accepts `after` and `limit` query values.
Create accepts `{rel_path, kind?, password?, expires?}` only: identity comes
from the unified maild cookie; `account`, `blob` and root overrides are rejected.
The live session epoch must match. Mutations require `X-CSRF-Token` equal to
the sealed token and reject a mismatched Origin/Referer. Create returns 201;
list/revoke return 200. Missing identity returns 401, CSRF failure 403, invalid
arguments 400, missing catalogue 404, resource saturation 503. Extractor errors
use axum's 400/413/415/422 statuses. All management responses are private/no-store.

Passwords contain 1..72 UTF-8 bytes and hash with bcrypt cost 12 on one creation
worker. Verification has four blocking workers and permits stored costs 4..14; malformed or more
expensive hashes fail closed. Cancellation retains worker admission until the
blocking computation ends. Passwords are never accepted in a query string.

## Identity and roots

The catalogue uses the exact canonical email from the unified maild session
cookie as `account TEXT`. This is an explicit C1 deviation: webd has no stable
numeric maild-account lookup today. `maild_account_id INTEGER NULL` reserves the
future mapping. An email change therefore needs an explicit catalogue/root
migration; webd never guesses that two emails or a CMS user ID are one account.

Configure roots in the existing native `node.conf.mix` format:

```mix
webd: {
  shares: {
    roots: {
      "user@example.test": "/srv/files/user"
    }
  }
}
```

This is the `webd.shares` block (the conceptual `[shares]` section of webd's
configuration), not a new configuration file or TOML parser. At startup webd
logs and skips roots that are relative, contain `..`, or are not existing
directories. Missing roots deny path shares. Roots never come from requests.
Keep these trees outside public document roots and readable by the service.

Startup canonicalises each configured root and pins an open directory descriptor.
Reads use `cosmix-lib-files::rooted_read::ReadRoot::open_regular`: Linux `openat2`
resolves beneath that descriptor, rejecting all descendant symlinks, including
links to files inside the root. Only regular files are returned. Renaming or
replacing the root pathname cannot redirect an existing root handle. Relative
paths reject absolute paths, empty components, `.` and `..`.

This requires Linux with `openat2` support (Linux 5.6 or later). Unsupported
kernels/platforms or failed root opens skip the root at startup; there is no
path-check/open fallback. The configured root itself may be a symlink because
it is canonicalised before opening; descendant symlinks are always refused.

## Catalogue contract

Only `kind=file` is supported: either a jailed relative path or a validated
local blob reference. `dir` and `drop` creation is refused with
`invalid_arguments:`. This is a deliberate reduction of C6; it is not yet a
complete replacement for existing directory links or file-drop links.

`rel_path` and `blob` are nullable, with a database CHECK requiring exactly one
non-null target. `blob` contains the complete reference JSON (`blob`, `size`,
`mime`, optional `name`, `origin`); only lowercase `b3:` IDs are accepted.
Unknown kinds or malformed stored targets cannot resolve.

The migration is transactional and retains tokens, paths, passwords, expiry,
revocation and counters. Legacy numeric `account_id` values move to
`maild_account_id`, leaving `account=NULL`. These unmapped rows are retained but
unservable until explicitly assigned their correct email; no identity is invented.

Resolution checks revocation and expiry before target access. Password work must
run off the DB lock, followed by a fresh gate lookup and comparison with the
verified hash. New tokens carry 160 random bits. Management lists are bounded to
100 entries and cursor-paginated by token; password hashes are never returned.

`download_count` is best-effort download-start telemetry: increment once when
body bytes first emit, never for HEAD, denial or an empty body. It does not prove
receipt of a complete file.

Blob-share creation is Bus-only and pins before publishing a token.
The owner is `webd:share:<first-16-hex-of-BLAKE3(primary-fqdn)>`; no share pins are
released this arc, including on revoke or expiry. Reconciliation is deferred.

Share error tokens are `not_found`, `expired`, `revoked`, `unauthorized` and
`invalid_arguments:`. Public responses collapse missing/revoked/expired to 404
and password failures to 401. All public denial bodies use `not_found` (416
remains empty); management keeps detailed tokens. Public lane errors expose
only the fixed prefix token, with diagnostic details written to the log.
Database faults remain internal failures.

## Local lane adapter

`blob_lane::local` loads the current `node.broker_handle` for each operation.
Discovery calls the local `blobd` citizen's `blob.props.get {path: "lane"}`;
only a socket address is accepted. A reference's `origin` is provenance, never
a request destination. The dedicated reqwest client disables redirects,
proxies and decompression. A missing local blob returns `not_present:`; serving
does not trigger a cross-node fetch.

One shared `Lane` instance admits at most eight operations, without a waiting
queue. GET bodies retain admission through completion or drop. Bus calls have
a 10-second deadline; lane headers and each pending upstream body read have a
30-second deadline. Public downloads additionally enforce the timer-driven
30-second progress deadline and one-hour lifetime described above. Public
connection limits remain owned by the listener. Catalogue and password work have
their own bounded admission pools. Revocation does not cancel an already-authorised body.

GET/HEAD forward Range and check 200/206/416 against the reference size and the
requested extent. Single ranges follow blobd's semantics: malformed/multiple
ranges are ignored; valid out-of-bounds ranges return 416. The adapter checks
Content-Length, Content-Range and Accept-Ranges, rejects encoded/chunked byte
responses, and aborts a body that is short or exceeds the declared extent.
416 diagnostics are discarded and replaced by an empty body with validated
Content-Range. Only Content-Type (from the validated reference), Content-Length,
Content-Range and Accept-Ranges leave this adapter; public attachment/security
headers are added by the share route.

The media-reference adapter hashes the complete bytes with BLAKE3, checks
`blob.stat` first, and recovers or adds an owner pin without uploading existing
bytes. `blob.stat` has no name field today, so recovered references omit the
optional name. Missing content uses hash-addressed PUT with advisory quota
preflight, bounded upload chunks and an upload progress watchdog. Both 201 and
the present-hash early 200 are accepted, but the returned hash and size must
match. Reference replies are capped at 64 KiB and 30 seconds. Pin publication
is separate from catalogue publication. See [media references](media-references.md)
for row-first dual-write and the `webd.media.ref` retry verb.

Lane errors retain `lane_unavailable:`, `quota:`, `lane:`, `not_present:` and
`verify_failed:`. Saturation is `lane: transfer pool full (8)`; invalid caller
references are `invalid_arguments:`. An error after response headers aborts
the body stream rather than attempting a second HTTP status.

The public handler maps lane failures to 502, except `lane_unavailable:` (503).
Its own resource saturation is 503 `busy:`; database failures are 500 `internal:`.
`not_present:` therefore means 502 for an existing token whose local bytes are
missing, distinct from the 404 for an unknown/revoked/expired token.
