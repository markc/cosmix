# Public shares

P5 is being delivered in slices. The catalogue and root configuration are
implemented first; this checkpoint does **not** expose HTTP or Bus share routes.

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

Blob-share creation will be Bus-only and must pin before publishing a token.
The owner is `webd:share:<first-16-hex-of-BLAKE3(primary-fqdn)>`; no share pins are
released this arc, including on revoke or expiry. Reconciliation is deferred.

Share error tokens are `not_found`, `expired`, `revoked`, `unauthorized` and
`invalid_arguments:`. Public responses collapse missing/revoked/expired to 404
and password failures to 401. Database faults remain internal failures.

## Local lane adapter (checkpoint foundation)

`blob_lane::local` loads the current `node.broker_handle` for each operation.
Discovery calls the local `blobd` citizen's `blob.props.get {path: "lane"}`;
only a socket address is accepted. A reference's `origin` is provenance, never
a request destination. The dedicated reqwest client disables redirects,
proxies and decompression. A missing local blob returns `not_present:`; serving
does not trigger a cross-node fetch.

One shared `Lane` instance admits at most eight operations, without a waiting
queue. GET bodies retain admission through completion or drop. Bus calls have
a 10-second deadline; lane headers and each pending upstream body read have a
30-second deadline. Backpressure from a slow downstream keeps its permit: this
is an upstream I/O idle deadline, not a total public download lifetime. Public
connection deadlines and admission of catalogue/password work are wired with
the HTTP handler in later slices.

GET/HEAD forward Range and check 200/206/416 against the reference size and the
requested extent. Single ranges follow blobd's semantics: malformed/multiple
ranges are ignored; valid out-of-bounds ranges return 416. The adapter checks
Content-Length, Content-Range and Accept-Ranges, rejects encoded/chunked byte
responses, and aborts a body that is short or exceeds the declared extent.
416 diagnostics are discarded and replaced by an empty body with validated
Content-Range. Only Content-Type (from the validated reference), Content-Length,
Content-Range and Accept-Ranges leave this adapter; public attachment/security
headers are added by the later share route.

The media-reference foundation hashes the complete bytes with BLAKE3, checks
`blob.stat` first, and recovers or adds an owner pin without uploading existing
bytes. `blob.stat` has no name field today, so recovered references omit the
optional name. Missing content uses hash-addressed PUT with advisory quota
preflight, bounded upload chunks and an upload progress watchdog. Both 201 and
the present-hash early 200 are accepted, but the returned hash and size must
match. Reference replies are capped at 64 KiB and 30 seconds. Pin publication
is separate from catalogue publication; these foundations do not yet dual-write
media or expose a `webd.media.ref` verb.

Lane errors retain `lane_unavailable:`, `quota:`, `lane:`, `not_present:` and
`verify_failed:`. Saturation is `lane: transfer pool full (8)`; invalid caller
references are `invalid_arguments:`. An error after response headers aborts
the body stream rather than attempting a second HTTP status.
