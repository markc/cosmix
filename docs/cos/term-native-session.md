# Term native-session launch handoff

Term uses a separate verified Unix connection for the native-session profile.
The diagnostic `term` service remains independent. The system broker account
defaults to `cosmix-noded`; trusted launch configuration can select a different
account with `COSMIX_BROKER_ACCOUNT`. The endpoint follows the client library's
explicit/config/discovery/system order and always undergoes ownership and peer
credential verification. An unavailable profile leaves ordinary panes usable;
once identity is established, a failed grant launch reports an error.

Term retains its Ed25519 key for challenge-based resumption. A separate actor
renews every five seconds. Replacement parent records, including after broker
restart, receive fresh grants for live panes using their retained public keys.
Grant expiry notices and lifecycle gaps trigger reconciliation. Re-grants
advance the pane generation within the same parent instance. The child proves
with its own retained private key; Term never receives that key back.

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
Term also sets CLOEXEC on any bootstrap descriptor it inherited, before its
first spawn. The memfd name `cosmix-session` identifies its purpose through
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

Pane close invalidates local session state before queuing cleanup. The cleanup
worker waits for a bounded revoke attempt before terminating the child. Child
exit also queues revocation directly from the PTY event. Shutdown attempts
child revocations followed by parent revocation; connection detection and
broker lease/resumption deadlines bound lost notifications. This slice does
not install protected command handlers; their authorisation gate is S4.

## Verification inventory

Source tests cover memfd layout round-trip and all four seals; a real PTY
helper checks that the mapped memfd appears exactly once and is absent from a
second child while Term's descriptors remain open. Real in-process noded tests
cover pane and Term revocation, the PTY exit callback, tab close before queued
cleanup, same-epoch resumption, lost create-ACK reconciliation, broker bounce
with the retained public key and parent-key hash, and expiry-driven generation
advance while five-second renewals keep the parent alive. The expiry test uses
the broker's real clock and takes at least 31 seconds.

The tests-only support crate embeds production noded modules by path. Each
fixture owns a separate runtime so a bounce drops accepted sockets and all
background tasks, not just listeners. It neither changes noded's production
API nor simulates its command engine. The Mix end-to-end seam is explicitly
ignored pending the child slice. Test and clippy results are supplied by the
orchestrator; this document records implementation and test inventory only.
