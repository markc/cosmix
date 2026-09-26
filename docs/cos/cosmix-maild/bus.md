# cosmix-maild Bus surface

## Service

The daemon registers as Bus service `maild`.

Broker registration retries with exponential backoff. SMTP, IMAP, JMAP, and DAV continue serving while the broker is unavailable. After a disconnect, the daemon reconnects and registers again.

## Request and response convention

Action arguments resolve in this order:

1. JSON in the `args` header
2. A non-empty JSON body
3. The client library's parsed `args` value

Individual verbs may also accept named headers such as `email`, `account`, `by`, `limit`, or `dry_run`. Where both forms are accepted, the header wins.

Success uses `rc = 0`. Caller, validation, and engine failures use `rc = 10` with a JSON error body. Property routing preserves its own `0`, `10`, and `20` return codes.

## Account verbs

| Verb | Arguments | Result |
|---|---|---|
| `maild.accounts.seed_mailboxes` | Optional `email` | Idempotently create Inbox, Drafts, Sent, Junk, Trash, and Archive |
| `maild.accounts.seed_content` | Required `email` | Idempotently create Posts and Pages content folders |
| `maild.accounts.revoke_tokens` | Required `email` | Revoke all live bearer tokens for the account |
| `maild.accounts.verify` | `email`, `password` | Return `valid` without exposing the stored hash |
| `maild.accounts.lock` | Required `email` | Disable password authentication and preserve the hash |
| `maild.accounts.unlock` | Required `email` | Restore the preserved password hash |

An unknown account and a wrong password both produce `valid: false` from `verify`. Lock and unlock are idempotent.

## Attachments and message exports

| Verb | Arguments | Read-only |
|---|---|---|
| `maild.attachment.list` | `account_id`, `email_id` | yes |
| `maild.attachment.ref` | `account_id`, `email_id`, `part`, optional `name` | no |
| `maild.message.ref` | `account_id`, `email_id` | no |

These accept a positive integer account ID and a message UUID. Every call
resolves that message through the account-owned mail store before reading
bytes or reference rows. Unknown/foreign messages and absent parts use
rc 10 `not_found: message or part`; malformed arguments use
`invalid_arguments:`. Inspection limits and failures use `too_large:` and
`unreadable:`. Nothing is written by inspection.

`attachment.list` replies with
`{"email_id":"<UUID>","blob":"b3:<message hash>","parts":[...]}`.
Each part has `part`, `mime`, decoded `size`, nullable `disposition`, and
`is_attachment`; optional `name` and `content_id` are omitted when absent.
An exported part also has `blob: "b3:<part hash>"`. The list uses the same
root-`1` MIME paths as JMAP, including embedded-message children. It does not
publish a partial list when inspection fails.

`attachment.ref` extracts and exports one part. `message.ref` exports the
whole RFC 5322 message with the upload MIME hint `message/rfc822`. Successful
exports return the canonical reference fields `blob`, `size`, `mime`,
optional `name`, `origin`, plus `email_id` and (for a part) `part`.
The owner is `maild:<account_id>`. Names and MIME are hints: blobd retains
first-writer metadata, and replies preserve that metadata.

Validated references persist in the global `attachment_refs` and
`message_refs` tables, keyed by account, item and message hash (plus part
for attachments). Repeats return the saved reference without contacting the
lane. Concurrent exports converge on one row and the same owner/hash pin.
Pins and reference rows are retained after mail/account deletion; release
belongs to later reconciliation. An interrupted export or a DB failure
after upload can leave a pin without a reference row; retry is safe and
does not remove that pin. No DB mutex is held across HTTP.

Exports run in a session-owned pool of eight tasks outside serial Bus
dispatch. A full pool immediately returns rc 10
`busy: maild blob transfer pool is full (8)`. Reconnect or session shutdown
cancels the tasks. Lane failures use `lane_unavailable:`, `quota:`, `lane:`,
`not_present:` or `verify_failed:`; all are rc 10. There is no detach,
message rewrite, shared-root access or implicit remote fetch.
`message.ref` is export, not import: JMAP `Email/import` still requires an
upload UUID, and `/jmap/blob` still reads maild's own MDS store.

## Legacy blob migration

`maild.blob.migrate {apply?: false, account_id?, cursor?: 0, limit?: 500}`
runs on the daemon's own store handle. It accepts at most 500 rows per call;
pages also end after 64 MiB of declared source data (one large row can exceed
that budget). Copy and verification stream through a fixed-size buffer.

The default is read-only inspection: no set provisioning, CAS writes, aliases
or queue changes. `apply: true` verifies each legacy file's hash and size,
copies it into MDS, preserves its UUID alias and adds a non-expiring holding
item in `__upload_staging__`. The membership's sole tag is
`maild:legacy-blob:<UUID>`; alias expiry and temp-item fields are NULL. This
marker and alias commit together. Repeating a page creates no extra holds.
The holds intentionally survive upload expiry and mail retention until a
later explicit legacy retirement; this verb deletes no legacy rows or files.

Verified rows backfill `smtp_queue.blob_hash`; the old UUID is retained.
Existing conflicting aliases or queue hashes are refused. A retry after a
queue update failure reuses the committed hold and finishes the queue update.

Replies contain `done`, `next` (a numeric rowid cursor or null), and per-account
`planned`, `migrated`, `already_migrated`, `missing`, `corrupt`, `conflicting`
and `failed` counts. Pass `next` as `cursor`, retaining the same account filter
and apply mode. `done` means enumeration finished, not that every row passed.
Rerun from cursor 0 after repairing failures. Up to 20 bounded diagnostics are
returned. All successful rows return rc 0; any failed row returns rc 10 with
`migration:` and the page report intact. Validation errors use
`invalid_arguments:`. Row diagnostics distinguish `missing:`, `corrupt:`,
`conflicting:` and `unreadable:`; other storage errors count as `failed`.

This is a Bus maintenance operation, not a local CLI subcommand. No second
MDS instance is opened and no blobd service or shared root is involved.

## Rules and Bayesian verbs

| Verb | Arguments | Result |
|---|---|---|
| `maild.rules.reload` | None | Reload the configured pack and return load metadata |
| `maild.rules.stats` | Optional `top_n` | Return pack metadata and persistent verdict and rule-hit counters |
| `maild.rules.explain` | Envelope and base64 message | Explain rule evaluation without delivering |
| `maild.bayesian.stats` | `account_id` or `email` | Return per-account corpus statistics, read-only, echoing the resolved `account_id` and `email`; an account with no row is refused |
| `maild.bayesian.classify` | `account_id`, `message_b64` | Classify without recording a training label |
| `maild.bayesian.train` | `account_id` or `email`; `email_id` or `message_id`; `class` (`spam` / `ham`) | Train one stored message through the Junk-move path; returns `result` (`applied` / `already_labeled`) |
| `maild.bayesian.untrain` | `account_id` or `email`; `email_id` or `message_id` | Remove the message's training label and reverse its counts; returns `removed` (`spam`, `ham`, or null) |

`maild.bayesian.stats`, `train` and `untrain` accept `account_id`, `email` or `account` (`rebuild` and `rebuild_status` take only `account_id`); `account` is the name the `maild.stats.*` verbs use. The address match is exact and case-sensitive, with no alias or `+tag` expansion, so an unmatched address fails closed. An unknown account is reported as `account not found`, the same text `maild.bayesian.rebuild` uses. The `maild.stats.*` verbs report `no such account: <address>` instead.

`train` and `untrain` refuse any key outside those listed (for example `dry_run`) with `rc=10` rather than ignoring it.

`maild.rules.explain` accepts:

```json
{
  "account_id": 42,
  "envelope_from": "sender@example.com",
  "envelope_to": ["admin@example.com"],
  "peer_ip": "192.0.2.20",
  "message_b64": "RnJvbTogc2VuZGVyQGV4YW1wbGUuY29tDQoNCkhlbGxvDQo=",
  "mail_auth": null
}
```

`account_id` may be omitted for engine defaults. It may otherwise be a non-negative JSON integer or an all-digit string. `mail_auth` is reserved; the handler currently synthesises a no-DNS verification result for explanation.

`maild.rules.stats` returns at most 256 rule entries by default and clamps `top_n` to 4096. `top_n: 0` returns rule cardinality without the per-rule map.

## Search and statistics verbs

| Verb | Arguments | Result |
|---|---|---|
| `maild.search.rebuild` | Optional `email` | Rebuild search rows for one or all accounts |
| `maild.stats.mailboxes` | Required `account` or `email` | Per-mailbox totals, unread counts, and bytes |
| `maild.stats.account` | Required `account` or `email` | Account-wide storage and message roll-up |
| `maild.stats.online` | None | IMAP connection counts and recent JMAP activity |
| `maild.stats.server` | None | Server-wide storage, queue, connection, and uptime data |
| `maild.stats.top` | Optional `by`, `limit` | Rank accounts by size or message count |

`maild.stats.top` defaults to `by: "size"` and `limit: 10`. `by` also accepts `"count"`; the limit is clamped to the range 1 through 1000.

Search rebuilds process accounts sequentially. A failure for one account is reported without preventing attempts for the remaining accounts.

## Retention verbs

| Verb | Arguments | Result |
|---|---|---|
| `maild.retention.status` | None | Current policy and last-sweep state |
| `maild.retention.run` | Optional `account`, `dry_run` | Run one sweep immediately |

Status is read-only. `run` requires the Bus sender to appear in `retention_operators`; an empty allowlist denies every caller.

The property defaults are inert: Junk and Trash windows are zero, no accounts are armed, and `dry_run` is true.

## DKIM and TLS verbs

| Verb | Arguments | Result |
|---|---|---|
| `maild.dkim.generate` | `domain`, `selector`, optional `algorithm` | Write a new key, update domain state, rebuild the signer, and return a DNS record |
| `maild.dkim.rotate` | `domain`, `selector` | Promote an existing substrate-managed selector |
| `maild.dkim.retire` | `domain`, `selector` | Remove a non-active substrate-managed selector |
| `maild.tls.reload` | None | Rebuild and atomically swap the SNI resolver |

DKIM mutations operate on substrate-managed domains. Operator-managed startup rows are not modified. Key writes are atomic and private; domain updates use versioned replacement.

TLS reload reads startup and substrate identities, validates certificate/key pairs, updates the `maild.tls_identities` projection, and clears the server-config cache. A rebuild failure leaves the previous resolver serving.

## Virtual-token verbs

| Verb | Arguments | Result |
|---|---|---|
| `maild.vtoken.mint_opaque` | Account, sender, verification strength, service, and optional state | Mint a sender-locked opaque address and return its plaintext token once |
| `maild.vtoken.list_opaque` | None | List stored opaque-token rows without secret PIN fields |
| `maild.vtoken.lookup_opaque` | `token_hmac` | Read one stored row |
| `maild.vtoken.disable_opaque` | `token_hmac` | Disable a stored token |

The global path requires the Bus sender in `vtoken_operators`. The delegated path requires a top-level `$cosmix_delegation` envelope and a sender in `vtoken_delegated_peers`. A delegated peer cannot fall back to the global path.

Opaque plaintext tokens are returned only when minted. Storage uses an HMAC derived from a server secret rather than the raw token.

## Property verbs

Commands with the `maild.props.` prefix are bridged to the property router. The crate uses:

- `maild.props.get`
- `maild.props.list`
- `maild.props.set`
- `maild.props.delete`
- `maild.props.watch`

The routed namespaces are `accounts`, `account_overrides`, `aliases`, `domains`, `engine_config`, `retention`, `tls_identities`, and `log`.

Namespace schemas and hooks enforce record shape, canonical keys, merge behaviour, cross-record constraints, secret redaction, lifecycle work, and write policy.

## Published topics

### `maild.verdict`

One event is emitted after an inbound message is durably delivered. The event carries routing, rules, Bayesian, mail-auth, score, and stamp information used by subscribers.

Publication is best-effort after commit. Delivery is not rolled back if the topic cannot be published.

### `maild.props.records.changed`

Property changes are published without broker retention. `maild.props.watch` obtains a broker subscription grant before live delivery begins.

## Availability

Bus runs in a sibling task to the mail protocols. Broker connection loss removes the management and event surface temporarily but does not stop mail serving.
