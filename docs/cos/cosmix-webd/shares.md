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
