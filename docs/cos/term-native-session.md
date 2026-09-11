# Term native-session launch handoff

Term uses a separate verified Unix connection for the native-session profile.
The diagnostic `term` service remains independent. The system broker account
defaults to `cosmix-noded`; trusted launch configuration can select a different
account with `COSMIX_BROKER_ACCOUNT`. The endpoint follows the client library's
explicit/config/discovery/system order and always undergoes ownership and peer
credential verification. Identity establishment runs on the actor. Before the
first TabSet open, main waits once for at most 900ms for a ready bundle; healthy
startup can therefore bind pane 1. An unavailable profile or failed grant leaves
ordinary panes usable, including Ctrl+Shift+T.
`term.session {}` reports per-pane binding status and the latest provisioning
diagnostic; these are diagnostic snapshots, not evidence of live authority.

The actor pre-provisions one keypair, grant and sealed LaunchFd for the next
pane ID. Opening or splitting consumes a ready bundle without RPC, channel
waiting or memfd construction on the GUI/Bus thread. If none is ready, the
pane opens unbound (graphics-only), logs the reason and records it for
`term.session`. The next bundle is provisioned in the background, including
from the five-second renew tick. Its conservative local CLOCK_BOOTTIME expiry
starts before grant creation, includes suspend and never extends on fetch.
An unused bundle (or a missing bundle for the next slot) refreshes only while
user presence is recent. Physical keyboard input, pointer/menu actions and
window focus gain update a coarse atomic last-activity timestamp. Bus mutations
do not manufacture presence. The presence window is five minutes, and each
slot has a minimum 60-second interval between mints. Once idle, the actor holds
the pool until activity returns. This policy applies to the unconsumed pool,
not to live panes. No later pane open waits for RPC or startup readiness.
Unbound running panes are not retroactively bootstrapped.
The mint floor starts at the create acknowledgement (or uncertain timeout),
so RPC latency cannot shorten the interval between issued names.

A bundle needs at least ten seconds remaining at consumption: four two-second
connect/hello/challenge/prove budgets plus spawn slack. Short-window bundles
take the unbound fallback and the pool subsequently refreshes under presence
and its mint floor. Reconnection and reconciliation first withdraw shared
readiness and invalidate the old consumption latches, before any await. The
actor retains only the unused key and republishes a newly sealed descriptor
and fresh latches under the reconciled scope. An old generation-2 descriptor
cannot be consumed during replacement with a generation-1 broker incarnation.

No-name failures clear the provisioning latch so the next tick can retry.
Definitive create refusals release the provisional mint floor. A missing or
malformed create ACK retains the floor and hold because a name may have been
issued. Memfd delivery failure retains the known grant and key: the next tick
retries delivery of that grant without spending another name.

The 32-pane admission limit happens to equal noded's default per-parent pending
grant quota. Look-ahead provisioning spends one slot too; configured lower
quotas can refuse it. This reports “broker pending-grant quota exhausted” and
opens an unbound pane rather than failing pane creation.

Term retains its Ed25519 key for challenge-based resumption. A separate actor
renews every five seconds. Replacement parent records, including after broker
restart, receive fresh grants for live panes using their retained public keys.
Re-grants advance the pane generation within the same parent instance. The child proves
with its own retained private key; Term never receives that key back.

A never-enrolled pane receives its initial grant and at most one new grant per
external recovery event (successful reconnect, lifecycle gap or explicit pane
restart). Expiry alone holds it with a visible diagnostic. For a previously
enrolled pane, expiry schedules at most three automatic attempts with delays
of 120, 240 and 480 seconds after successive expiries/failures. Reconnect/gap
re-arms recovery but never resets the child's 60-second mint floor, including
across broker epochs. An external re-grant blocked by that floor is deferred;
it is a deliberate wait, not a recovery error. Attempts are charged before
mutation, including uncertain
ACKs and quota refusals. Existing grants are reconciled by key before minting;
their own state, expiry identity and conservative local window are checked
independently of record state. Consumed grants on attached/suspended records
are retained attachments, not launchable pending grants. Broker-domain
absolute timestamps are never compared with the client's time namespace.

This bound is deliberate under BROKER-019's quota qualification: noded retains
issued names for the whole epoch (65,536 node-wide). Unbounded 30-second
re-grants for 32 idle panes would exhaust that budget in roughly 17 hours.
For one continuously refreshed unused pool slot, the 60-second floor permits
at most 1,440 issued names in a half-open 24-hour period, including the initial
mint; idle periods issue none. Against 65,536 names per epoch, nonstop refresh
alone could still use the shared budget in roughly 45 days. Parent allocations,
explicit new-pane launches and live-child recovery also spend names. Each live
child's reconnect/gap re-grants now obey the same floor, so event storms cannot
mint repeatedly within a minute. The broker's quota remains the final bound.

## Sealed memfd version 1

The nonsecret environment marker `COSMIX_SESSION_FD` is the decimal descriptor
number. The anonymous memfd contains exactly these bytes:

| Offset | Encoding |
|---|---|
| 0 | Version byte `01` |
| 1–4 | Public JSON length N, unsigned 32-bit big-endian, at most 16384 |
| 5–(4+N) | UTF-8 JSON object with `grant` and `record`, the typed `grant.create` result using BUS-016 encodings |
| (5+N)–(36+N) | 32 raw Ed25519 seed bytes, not the expanded 64-byte key |
| 37+N | Exact EOF; no trailing bytes |

Term writes the seed separately from the public JSON, seals shrink/grow/write,
then seals the seal set. Both reserved source and target descriptors are
CLOEXEC in Term. The pinned teletypewriter patch duplicates source onto target
only after fork, closes source in the child, and leaves PTY stdio intact. Term
closes both descriptors immediately after spawn. Temporary seed arrays and the
child signing key use zeroize-on-drop. Seals protect integrity, not secrecy.
Term also sets CLOEXEC on inherited bootstrap descriptors numbered at least 3,
before its first spawn; malformed markers never change stdio.
The memfd name `cosmix-session` identifies its purpose through
procfs; this metadata disclosure is accepted. Anonymous memfd pages may be
swapped: protection against privileged memory inspection or swap extraction is
outside this threat model. Sealing is not encryption or memory locking.

The child MUST seek to offset zero or use `pread`; inherited descriptors share
an open-file offset, so it must not depend on Term's last offset. It SHOULD
verify `F_GET_SEALS` contains SHRINK, GROW, WRITE and SEAL before parsing.
`record.lease_remaining_ms` is stale by construction and informative only;
it is never a live-authority deadline.

Construct the typed client's `ExpectedScope` from the retained launch scope:

| Expected field | Source |
|---|---|
| `unix_uid` | `record.owner_uid`, checked against the child's effective UID |
| `parent_key_hash` | `Some(grant.parent_key_hash)` |
| `pane_id` | `record.pane_id` |
| `pane_high_water` | `record.pane_generation`, retained and advanced within a parent incarnation |
| `role` | `record.role` |
| `public_key_hash` | SHA-256 of the raw retained child public key |
| `capabilities_hash` | SHA-256 of `encode_capabilities(record.capabilities)` |
| `broker_epoch` | Fresh `session.hello` on the current verified connection |
| `purpose` | Enrol for a pending child; resume for an existing attachment |

Do not construct expectations by copying a received challenge. The key
selector's fixed wire tag is not a purpose claim: noded derives the returned
purpose. `ChallengeResult::sign` checks it and the current broker epoch against
the independent expectations before signing. A broker restart requires a fresh
hello and parent-confirmed replacement scope, retaining the parent key hash.

The next Mix slice must read and validate this layout before hooks or user
source, close the descriptor, remove the marker, retain the signing key outside
the language surface, and use authenticated key-selected challenges for recovery.
The full child-proves-over-real-PTY acceptance seam is
`mix_child_bootstrap_proves_end_to_end`.

Pane close invalidates local session state before removing metadata or queuing
cleanup. The cleanup worker waits up to three seconds for a revoke attempt.
That is best effort, not guaranteed acknowledgement: three RPCs can take six
seconds, plus queued work. Child
exit also queues revocation directly from the PTY event. Shutdown attempts
child revocations followed by parent revocation; connection detection and
broker lease/resumption deadlines bound lost notifications. Transport close is
bounded by the two-second RPC budget; actor shutdown has a three-second child
batch, two-second parent revoke and two-second close. Supervisor teardown waits
at most eight seconds and never joins an unfinished worker. Timeout diagnostics
name abandoned revokes or transport close and the remaining 15-second lease /
30-second window expiry. Stopping during connection establishment also reports
the skipped parent revoke. Failed child closes
retain public-key tombstones for retry; a 128-entry backlog stops provisioning
new grants (panes still open), rather than accumulating dead entries forever.
An epoch replacement drops obsolete dead entries. Unknown lifecycle fields
remain rejected and dropped-notice parse errors are logged. This slice does
not install protected command handlers; their authorisation gate is S4.

## Verification inventory

Source tests cover memfd layout round-trip and all four seals; a real PTY
helper checks that the mapped memfd appears exactly once and is absent from a
second child while Term's descriptors remain open. Real in-process noded tests
cover pane and Term revocation, an actual OS child exit through the PTY event
path, and metadata/PTY ordering probes under a paused real broker. They also
cover same-epoch resumption, lost create-ACK reconciliation, broker bounce with
the retained public key and parent-key hash, no-regrant expiry hold while renews
keep the parent alive, immediate ready/missing-bundle consumption, broker-down
Ctrl+Shift+T, lowered-quota diagnostics and saturated-UDS close/exit deadlines.
The expiry test uses the broker's real clock and takes at least 31 seconds.
A separate simulated-cycle test verifies noded's actual minted 30-second expiry
and counts real grant.create calls across the exhaustion horizon. Synthetic
expiry uses the real broker's revoke transition and the production notice/retry
paths; it does not simulate broker RPC replies. The memfd helper also verifies
Term's startup quarantine prevents a further child's inheritance. Typed-client
integration tests reject wrong proof purpose and broker epoch before signing.

The tests-only support crate embeds production noded modules by path. Startup
tests exercise first-open startup through main's Supervisor/TabSet path
with a healthy and an absent broker, without test-manufactured readiness. New
regressions cover short-window rejection; generation-2 descriptor withdrawal
before delayed reconnect and republication at generation 1; free retry after
quota/fetch/memfd failures versus uncertain create ACKs; gap-storm mint floors;
and a full simulated day of active-presence pool mints followed by an idle day.
The wall-clock broker-bounce test allows the deliberate 60-second mint floor.
The PTY ordering probe signals entry before its broker RPC, and stdio markers
are rejected before any quarantine syscall.

Each fixture owns a separate runtime so a bounce drops accepted sockets and all
background tasks, not just listeners. It neither changes noded's production
API nor simulates its command engine. The Mix end-to-end seam is explicitly
ignored pending the child slice. Test and clippy results are supplied by the
orchestrator; this document records implementation and test inventory only.
