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

## Rules and Bayesian verbs

| Verb | Arguments | Result |
|---|---|---|
| `maild.rules.reload` | None | Reload the configured pack and return load metadata |
| `maild.rules.stats` | Optional `top_n` | Return pack metadata and persistent verdict and rule-hit counters |
| `maild.rules.explain` | Envelope and base64 message | Explain rule evaluation without delivering |
| `maild.bayesian.stats` | `account_id` | Return per-account corpus statistics |
| `maild.bayesian.classify` | `account_id`, `message_b64` | Classify without recording a training label |

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

