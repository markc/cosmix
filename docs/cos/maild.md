# cosmix-maild — JMAP-native mail daemon

**`cosmix-maild` is a single-binary mail server: SMTP in and out, IMAP and JMAP for
clients, CalDAV/CardDAV for calendars and contacts, and a three-stage inbound
filter that verifies, rules-checks, and Bayesian-classifies every message.** It is
a first-class Bus citizen — everything an operator would reach for a CLI or config
reload to do is also an addressable Bus verb.

## What it is

One daemon that speaks the standard mail protocols on their standard ports, stores
everything in a content-addressable metadata store, and exposes its whole
management surface over Bus. JMAP is the native model; IMAP and SMTP are mapped
onto the same store. It is built from a small family of crates:

| Crate | Role |
|---|---|
| `cosmix-maild` | the daemon — protocol servers, filter pipeline, Bus surface |
| `cosmix-mds` | mail data store — per-container SQLite metadata + content-addressable blob CAS |
| `cosmix-maild-auth` | SPF / DKIM / DMARC / ARC / iprev verification and DKIM/ARC signing |
| `cosmix-maild-rules` | deterministic Sieve-style rule engine (stage two of the filter) |
| `cosmix-maild-bayesian` | Robinson-Fisher spam classifier, per-account (stage three) |
| `cosmix-mail` | a headless JMAP client app (an Bus citizen that reads/composes via a maild) |

## What it does

- **Inbound SMTP** on port 25 with opportunistic (or required) STARTTLS. Each message runs the three-stage DATA filter: `cosmix-maild-auth` verifies SPF / DKIM / DMARC / ARC / iprev and prepends an `Authentication-Results` header → `cosmix-maild-rules` returns `HardAccept`, `HardJunk`, or `Continue` → `cosmix-maild-bayesian` scores anything that continued and routes it to Inbox or Junk.
- **Submission** on SMTPS (465), signing outbound mail with DKIM and sealing forwards with ARC.
- **Client access** via IMAPS (993) and JMAP over HTTP, plus CalDAV/CardDAV for calendars and contacts — all backed by the same `cosmix-mds` store.
- **Spam training** and classification that live and update per account.
- **vtoken** — an address-as-RPC scheme where a minted token encodes a scoped capability into an email address (mint / lookup / disable / list over Bus).
- **Retention** — a background worker that ages messages out per policy.
- **DKIM key lifecycle** — generate, rotate, and retire signing keys per domain.

## Running it

```sh
/opt/cosmix/bin/cosmix-maild --config /etc/cosmix/maild/config.toml serve
```

Subcommands cover setup and operations: `migrate` (run DB migrations), `account`
(account management), `queue` (SMTP queue), plus per-account rule overrides. Run
under systemd as `cosmix-maild.service`, ordered `After=cosmix-noded.service` so
the broker exists first. Listen addresses default from `/etc/cosmix/node.toml`
(JMAP, SMTP, SMTPS, IMAPS) and are overridable in the per-daemon
`config.toml`, which also carries the `[[dkim.domain]]` signing config. Each
listen field accepts a single address or a list, for multi-homed binds.

## Interfaces

- **Protocol ports:** SMTP `25`, SMTPS submission `465`, IMAPS `993`, and JMAP over HTTP (dev default `127.0.0.1:8088`); CalDAV/CardDAV share the HTTP surface.
- **Bus management surface** — the operational verbs, grouped:
- accounts: `maild.accounts.seed_mailboxes`, `maild.accounts.seed_content`, `maild.accounts.revoke_tokens`
- stats: `maild.stats.server`, `maild.stats.account`, `maild.stats.mailboxes`, `maild.stats.online`, `maild.stats.top`
- rules: `maild.rules.reload`, `maild.rules.explain`, `maild.rules.stats`
- bayesian: `maild.bayesian.classify`, `maild.bayesian.stats`, `maild.bayesian.rebuild`, `maild.bayesian.rebuild_status`
- dkim: `maild.dkim.generate`, `maild.dkim.rotate`, `maild.dkim.retire`
- vtoken: `maild.vtoken.mint_opaque`, `maild.vtoken.lookup_opaque`, `maild.vtoken.list_opaque`, `maild.vtoken.disable_opaque`
- retention: `maild.retention.run`, `maild.retention.status`
- search / tls: `maild.search.rebuild`, `maild.tls.reload`
- **Property surface:** the SPEC 12 `maild.props.*` namespace (`list` / `set` / `delete` / `watch`), with change events on the `maild.props.records.changed` topic.

### Bayesian corpus rebuild

`maild.bayesian.rebuild` accepts `account_id` (an integer or digit string),
`snapshot` (optional, default `true`), `wait` (optional, default `false`), and
`allow_empty` (optional, default `false`). It reserves one in-memory job per
canonical account ID, enumerates the current mail store, and trains a per-job
`<account>/bayes.rebuild-<pid>-<unix_ms>.db` without touching the live corpus. An
exclusive non-blocking advisory lock on
`<account>/rebuild.lock` serialises rebuilds across maild processes; stale
per-job shadows are removed only while that lock is held.
Junk is Spam; Trash, Drafts, Sent, and upload staging are ignored. A mailbox
inherits the nearest ancestor's special-use role when it has none of its own;
otherwise it is Ham. User corrections made while enumeration or training runs
are replayed into the shadow corpus before it is validated and atomically
copied over the live corpus in one SQLite write transaction.

Mailbox enumeration is not one MDS snapshot. The fence and replay protect user
corrections: moves across the Junk boundary, which write a live label row. The
swap re-reads that correction set inside the same `BEGIN IMMEDIATE` transaction
used to replace the corpus. If the set changed, maild re-derives those items
from their current folder memberships, replays and revalidates the shadow, then
retries. Five consecutive conflicts fail the job. Membership-only changes after
enumeration — a deleted message, a move between non-Junk folders, or newly
delivered mail — are not corrections, write no label row, and are reflected by
the next rebuild. The corpus is folder state as of enumeration, plus every
correction made before the swap.

After the initial shadow validation and before the first replay read, `snapshot:
true` writes a consistent `bayes.pre-rebuild-*.db` rollback copy. A refused empty
rebuild creates no snapshot. Retention pruning runs only after a successful
swap, keeping the newest two snapshots. Aborted or failed jobs delete the shadow
database and leave the live corpus unchanged.

Before snapshot creation, after replay, and after each conflict retry, a
zero-message shadow fails with
`rebuild produced an empty corpus; live corpus left untouched (pass allow_empty:
true to replace it anyway)`. Set `allow_empty: true` only when deliberately
clearing the live Bayesian corpus.

`bayesian_rebuild_operators` in maild's `conf.mix` is an opt-in peer allowlist
for `maild.bayesian.rebuild`. Its semantics are deliberately the inverse of
`retention_operators`: EMPTY means open to every Bus peer, the deliberate
agentic-first default; NON-EMPTY admits only exact `cmd.from` matches.
`rebuild_status`, `stats`, and `classify` remain open regardless of this list.

The job body contains `state`, `started_at`, `finished_at`, candidate and trained
Ham/Spam counts, `replayed`, `already_labeled`, `skipped_missing`, `conflicts`,
`errors`, `last_error`, `snapshot`, and `ignored_mailboxes`. `wait: true` holds
maild's Bus dispatcher for the whole rebuild and is subject to the client's
60-second request timeout; use it only for tests or small mailboxes. Real
mailboxes should use `wait: false`, which returns the reserved `running` job
immediately, then poll `maild.bayesian.rebuild_status` with the same `account_id`.
Status returns the latest daemon-memory job, or `idle` when none has run since
startup.

## Where it fits

- Registers with and is addressed through [noded](noded.md), the Bus broker.
- Links the Bus protocol family (`cosmix-lib-bus`, `cosmix-lib-client`) from the [bus](https://github.com/markc/bus) repo, and substrate libraries `cosmix-lib-config`, `cosmix-lib-daemon` (TLS / ACME), `cosmix-lib-props-store`, and `cosmix-lib-log`.
- Parses its native `*.conf.mix` config through `cosmix-lib-mix` from the [mix](https://github.com/markc/mix) repo.
- `cosmix-webd` fronts webmail against maild's JMAP; `cosmix-mail` is a native client; both drive it over JMAP + Bus.

## See also

- [noded](noded.md) — the Bus broker maild registers with
- [libraries](libraries.md) — the substrate crates maild links
- [overview](overview.md) — the cos daemon family
- [bus messaging](https://markc.github.io/mix/#_man/bus.md) — the Bus primitives used to drive maild's management verbs
