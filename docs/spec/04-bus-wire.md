---
title: Bus wire format and command contracts
chapter: 4
version: 0.1.2
status: draft
date: 2026-09-10
---

# Bus wire format and command contracts

## Scope and authority

ABP means **Agent Bus Protocol**. Bus names the protocol family and its
implementation packages; AMP is the historical name. This terminology follows
the accepted naming decision and does not introduce a wire-version change.
ABP carries semantic control messages, discovery and structured events. It is
not a desktop frame, input-event or bulk media transport.

This candidate separates requirements from the source profile checked at
`96d12fdf`. Source inspection is not an integration-test result. Requirements
marked **intended** have not been established as enforced at every ingress.

## Single-message framing

**BUS-001:** Producers MUST encode ABP as UTF-8, with an opening `---\n`,
flat header lines, a closing `---\n`, and an optional body:

```text
---
bus: 1
type: request
id: 0192b3a4-5e6f-7890-abcd-ef1234567890
to: noded
command: noded.ping
---
```

In the current WebSocket profile, one text message is one ABP message. The
serializer does not emit `---\nEOM\n`. The native Unix-stream helper reads one
message until EOF; the sender must close its write side. Neither is a parser
for concatenated EOM-delimited messages. A body may contain markdown horizontal
rules or another ABP envelope without changing the outer WebSocket boundary.

**BUS-002:** Consumers MUST NOT advertise the old proposed EOM streaming,
resynchronisation or arbitrary log-concatenation behaviour as implemented.
A future streaming profile needs explicit escaping, truncation and size-limit
rules before use; the old claim that an EOM sentinel cannot collide with
arbitrary body text is not a sufficient framing contract.

**BUS-003:** Canonical header lines MUST use `key: value`, including the space
after the colon for an empty value. Keys MUST be non-empty and contain neither
whitespace nor a colon. Producers MUST NOT put newlines in keys or values or
repeat a key. Header names are case-sensitive; new standard headers SHOULD use
lowercase. Values remain strings unless the receiving contract assigns a type.
Inline arrays and objects MUST be valid JSON in the canonical profile. Bodies
are interpreted by the command contract, not by their first character.

The implementation uses a `BTreeMap<String, String>`: serialisation orders keys,
parsing trims header values and trailing body whitespace, and duplicate keys
overwrite earlier values. It does not preserve exact original bytes. Signing
or hashing an original document MUST use the original bytes or a separately
specified canonicalisation, not a parse/serialise round-trip.

## Parser and envelope validation

**BUS-004 (intended at canonical ingress):** Canonical consumers MUST reject a
non-empty parsing diagnostic report. Legacy-document import MAY accept a report
but MUST expose the rejected material. `parse_lenient` returns a message plus
`skipped_lines` and `json_parse_errors`; `parse_strict` rejects either report;
compatibility `parse` discards both. The broker and native client still call
compatibility `parse` at the checked baseline.

Strict parsing is not full envelope validation: it does not reject duplicate
keys, require a closed header block in every case, validate all key characters,
or establish identity, command arguments or authorisation. The shared
`validate` helper is diagnostic and must not be mistaken for an ingress gate.

**BUS-005:** A routed request contract MUST identify its `command`, `to`, and
correlation `id`; a response MUST correlate with the request and carry an
unambiguous result. Producers SHOULD include `bus: 1` and an appropriate
`type` (`request`, `response`, `event`, `stream`). UUID v7 is the preferred
external ID. Current minimal messages omit some of these headers and are
accepted; the generic parser does not enforce UUIDs or required fields.

| Header | Contract |
|---|---|
| `from` | Broker-canonicalised connection identity; not caller authentication by itself |
| `to` | Local service shorthand or parsed address |
| `command` | Exact handler command; namespace conventions below |
| `id`, `reply-to` | Request/response correlation; broker may rewrite its internal routing ID |
| `args`, `json` | Inline JSON where the receiving command defines it |
| `rc`, `error` | Result code and diagnostic; absent `rc` means success in the compatibility profile |
| `ttl` | Historical proposed request deadline; broker enforcement is not verified and MUST NOT be assumed |
| `broker_origin` | Recipient-broker delivery class, defined in chapter 05 |

**BUS-006:** Prefer `rc` values 0 (success), 5 (warning), 10 (application
error), 20 (severe failure). Consumers MUST preserve a peer's returned code;
the implemented Mix client distinguishes warning band 1–9 and errors ≥10.
Negative Mix statuses are local transport/timeout results, not ABP peer codes.
An RPC timeout does not prove that the recipient did not perform the operation.

## Addresses

**BUS-007:** The local address grammar is:

```text
sub.service.node[.bus]
service.node[.bus]
node.bus
```

Each label contains 1–63 lowercase ASCII letters, digits or hyphens, with no
leading/trailing hyphen. Total target length, including a federation suffix,
is at most 253 bytes. More than three labels before `.bus` are invalid.
The broker interprets `sub` only enough to route to `service`; the service
owns its meaning. Bare `service` is a local registry shortcut, not a
`BusAddress`. Registered-name rules are narrower than address-label rules.

**BUS-008:** `local-address@mesh.example` is a reserved cross-mesh target.
The suffix MUST be a lowercase ASCII FQDN with at least two labels, no
trailing dot and no `xn--` label under the current policy. The parser accepts
this syntax, but the current router MUST refuse it with RC 10 and
`cross-mesh routing not implemented`. Cross-node routing inside a mesh is a
different facility. `<service>@<node>` is not the current shorthand.
DNS/SRV projection of `.bus` remains optional operational tooling; registry
routing does not depend on public DNS delegation.

## Command contracts

**BUS-009:** New domain commands SHOULD use a service namespace followed by
documented resource/action segments, for example `maild.account.list`.
Existing contracts are exact strings, not an inferred universal grammar.
The broker extensions `topic.*`, `spec.get`, and runtime universals
`HELP`, `INFO`, `QUIT` are explicit exceptions. The old simultaneous claims
of lowercase-only commands and uppercase universals are replaced by these
scoped rules. `ui.*` and `menu.*` rendering vocabulary is historical.

**BUS-010:** Command documentation MUST specify where arguments live, their
schema, validation, result shape, side effects, idempotency and errors. JSON
bodies are legitimate: there is no rule forcing every command's parameters
into the `args` header. Missing, malformed and valid payloads SHOULD be
distinguished before mutation. The current generic native-client DTO can map
malformed JSON bodies to null; this remains a boundary-hardening gap.

Content verbs `open/close/get/set/list`, lifecycle verbs
`status/refresh/save/add/remove`, and operation verbs `start/stop/pause/resume`
are naming guidance. They do not grant a service undeclared capabilities.
Rust daemons commonly expose namespaced introspection; bare runtime
universals are guaranteed only by Mix serve mode. Do not fabricate a
`<service>.HELP` alias from the existence of `HELP`.

## Resource and confidentiality limits

**BUS-011:** Receivers MUST bound allocation before consuming arbitrary
input. The checked library has a 16 MiB message constant, an 8 MiB WebSocket
frame constant and a 4096 processed non-empty header-line cap. Repeated keys
and malformed lines count towards that cap. The native EOF helper enforces
16 MiB and a 10-second read deadline; the broker sets a 16 MiB WebSocket
message limit. Effective limits depend on the transport endpoint; the text
parser alone is not a message-byte gate. Topic payloads have a separate
1 MiB input cap.

**BUS-012:** Public examples and general diagnostic traffic MUST omit secrets.
Sensitive operations MUST define redaction and protected transport explicitly;
the textual format itself offers no confidentiality. WireGuard membership
does not validate request content or replace per-command authorisation.

## Native session wire profile (intended)

The `native-session: 1` direction is accepted; BUS-013–017 are proposed
contracts awaiting implementation and acceptance evidence. They do not describe
the checked source above. Chapter [05](05-broker-topics.md#native-session-identity-intended)
defines commands, identity state and policy.

**BUS-013 — Local transport (intended).** The profile MUST carry ABP in
WebSocket text messages over a Unix-domain `SOCK_STREAM` connection, using an
HTTP/1.1 WebSocket upgrade at `/ws`. One text message is one ABP envelope;
BUS-011 limits apply. Binary messages MUST be refused. This is a persistent
broker ingress, distinct from the existing EOF-framed Unix helper in BUS-001;
neither helper nor a separate control protocol substitutes for this ingress.
The broker MUST obtain UID, primary GID and peer PID using `SO_PEERCRED` on
the accepted socket. Caller-supplied provenance is not kernel identity.
`SO_PEERCRED` is a connect-time snapshot: later credential changes, including
setuid, do not retroactively change that connection's principal. This boundary
is accepted; obtaining a new principal requires a new connection.

The listener path MUST resolve as `cosmix_path(Run)/noded/bus.sock`. A system
unit MUST pin `COSMIX_RUN` to its system runtime root (`%t/cosmix`); node
configuration key `noded.unix_socket` MUST publish the absolute endpoint path.
Clients MUST use that key when present, otherwise `/run/cosmix/noded/bus.sock`;
they MUST NOT resolve the system socket through their own XDG runtime directory.
The directory MUST be traversable
by local users and writable only by the broker account or root; socket mode
is `0666`. Clients MUST verify protected endpoint ownership and server peer
credentials against the configured broker account before trusting metadata.
Explicitly configured development endpoints require the same ownership checks.
No fixed numeric broker UID is assumed.

The broker MUST advertise `"native-session":"1"` alongside BROKER-005's
extension entries in `noded.ping` only when this profile is available. An
advertisement returned over TCP does not make TCP eligible: Unix ingress is
mandatory for this profile. Grant-bearing clients MUST
require it and MUST NOT downgrade to TCP on any authentication failure. Legacy
TCP/WebSocket and D2 mesh clients remain supported without verified-UID authority.
Headless nodes use the same system listener; no desktop or per-user broker is
required. Shared wire/client types MUST NOT depend on cos-side configuration.

**BUS-014 — Trusted principal envelope (intended).** `broker_principal` is a
reserved, lowercase header containing one compact JSON object, at most 4096
UTF-8 bytes. A broker MUST strip every ASCII-case-insensitive spelling of this
header from untrusted input before stamping its own value. This applies to
requests, correlated responses, events and inner topic envelopes at every
delivery boundary; it MUST preserve BROKER-003/004 and responder-channel checks.
Mesh ingress MUST NOT import a claimed remote Unix UID as local authority.
Unverified TCP/mesh deliveries MUST omit this header. A direct broker control
reply is authenticated by its endpoint and pending-response association, not
by a fabricated Unix caller stamp.

Required object fields are `version:1`, `assurance` (`local-unix` or
`session-bound`), `owner_node` (configured node label), `unix_uid`, `unix_gid`
(u32 JSON integers), `peer_pid` (u32 integer, diagnostic only), `broker_epoch`
and `connection_id` (BUS-016 identifiers), and `session` (null for `local-unix`).
For `session-bound`, `session` contains the BROKER-021 identity record's
`record_id`, `instance_id`, `incarnation`, `role`, `parent_instance`,
`parent_incarnation`, `pane_id`, `pane_generation`, `binding_generation`,
`capabilities`, and `lease_remaining_ms` with BUS-016's u64 decimal-string
encoding. The broker MUST compute remaining milliseconds immediately before
delivery enqueue, as the minimum across the record and its live ancestors,
clamped at zero. A recipient MUST NOT turn this into a fresh
lease by adding it to receive time: queue delay could extend authority.
BROKER-020's correlated lease check provides the conservative local deadline
across time namespaces. Connection `assurance` is distinct from discovery's
`record_assurance` (BROKER-021).
Supplementary groups MUST remain unknown unless separately verified.

Recipients MUST require exactly one canonical header, a supported version and
valid typed fields on an authenticated broker transport before exposing a
trusted request context. Missing, malformed or duplicate-case metadata MUST NOT
gain authority. Future unknown object fields MAY be ignored. Discovery records,
raw JSON deserialisation and retained publisher stamps do not establish current
mutation authority; recipients MUST check live scope and generation.
Retained topic deliveries MUST preserve publish-time principal attribution,
not stamp the subscriber as publisher or refresh an expired publisher lease.

**BUS-015 — Bootstrap validation (intended).** Every `noded.session.*` request
MUST use `bus: 1`, `type: request`, `to: noded`, a non-empty ASCII token
`id` of at most 128 bytes, `native-session: 1`, and an exact command from
BROKER-018. ID bytes MUST be in `0x21..0x7e` (no whitespace); retained mutations
additionally require the monotonically increasing decimal ID in BROKER-018.
Arguments MUST be a JSON object in the body. Requests MUST be
bounded to 16 KiB total, with at most 32 header lines and 16 levels of JSON
nesting, before unbounded allocation. Parsing MUST reject missing envelope
delimiters, malformed headers, duplicate header keys (including ASCII case
variants), duplicate JSON members at any depth, unknown command arguments,
invalid UTF-8, trailing non-whitespace JSON, and wrong types or encodings.
Only `bus`, `type`, `to`, `id`, `native-session`, `command` and optional `from`
are accepted request headers after BUS-014 stripping. `from` has no authority.
Responses MUST explicitly carry correlated `id`, `command`, `type: response`,
`bus: 1`, `native-session: 1` and `rc`; absent `rc` is not success here.
These gates MUST run before identity/authority mutation; an identifiable failed
prove still consumes only its own connection's challenge slot (BROKER-019).
Conformance REQUIRES a
duplicate-preserving parse or raw pre-scan in `cosmix-lib-bus`: the existing
BTreeMap header parser and last-wins JSON parsing destroy duplicate evidence,
which a validation layer above them cannot recover. Nullable schema fields
MUST be present with JSON null when absent; omission is an error. A Serde
`Option<T>` default accepting a missing member does not satisfy this contract.
Legacy commands retain their existing validation contract.

**BUS-016 — Proof encoding (intended).** Keys are Ed25519 public keys (32 raw
bytes); proofs are standard Ed25519 signatures (64 bytes), not Ed25519ph.
Verifiers MUST reject invalid/non-canonical encodings and weak/small-order
keys or signatures using strict verification. Wire byte strings use lowercase
hexadecimal with exactly two characters per byte, without prefixes or whitespace.
All identifier fields below are independent random 16-byte values, encoded as
32 hex characters; the nonce is 32 random bytes. Optional identifiers are
present and JSON null when absent, never omitted, empty or an all-zero sentinel.
Generation,
pane-ID and millisecond fields are u64 values encoded in JSON as canonical
decimal strings (`0` or a nonzero digit followed by digits). UIDs are u32 JSON
integers. Generation counters begin at 1 and MUST NOT wrap; an unattached
pending record uses binding generation `0` as specified by BROKER-020.

The signed transcript is the following concatenation, in exactly this order.
`u16/u32/u64` mean unsigned big-endian integers; `LP(s)` means a u16 UTF-8 byte
length followed by exactly those bytes; no NUL or newline is added. `OPT16(x)`
is byte `00` for null, otherwise byte `01` followed by 16 decoded bytes.
`OPT64(x)` uses the same presence byte followed by a u64 when present.
`OPT32(x)` uses the same presence byte followed by 32 decoded bytes when present.

| Order | Field | Bytes |
|---|---|---|
| 1 | Domain/version | ASCII `cosmix.native-session.proof` followed by byte `00`, then u16 `1` |
| 2 | `purpose` | One byte: `01` = `enrol`, `02` = `resume` |
| 3 | `broker_epoch` | 16 decoded bytes |
| 4 | `connection_id` | 16 decoded bytes |
| 5 | `challenge_id` | 16 decoded bytes |
| 6 | `nonce` | 32 decoded bytes |
| 7 | `grant_id` | OPT16; required for enrol, null for resume |
| 8 | `record_id` | 16 decoded bytes |
| 9 | `instance_id` | 16 decoded bytes |
| 10 | `incarnation` | 16 decoded bytes |
| 11 | `unix_uid` | u32 |
| 12 | `parent_instance` | OPT16 |
| 13 | `parent_incarnation` | OPT16 |
| 14 | `parent_key_hash` | OPT32 of SHA-256 of parent's raw public key; present-null for Term |
| 15 | `pane_id` | OPT64 |
| 16 | `pane_generation` | OPT64 |
| 17 | `role` | LP of exact ASCII role |
| 18 | `public_key_hash` | SHA-256 of the 32 raw public-key bytes, 32 raw digest bytes |
| 19 | `capabilities_hash` | SHA-256 of capability encoding below, 32 raw digest bytes |
| 20 | `binding_generation` | u64 proposed attachment generation |
| 21 | `grant_expires_ms` | OPT64; required for enrol, null for resume |
| 22 | `challenge_expires_ms` | u64 |

Capability encoding is u16 count followed by LP of each capability token,
sorted by ascending ASCII bytes, without duplicates. BROKER-023 defines the
tokens; no JSON, whitespace or separators enter this encoding. Non-null hashes
are represented as 64 lowercase hex characters in challenge JSON. Deadlines
are broker-host `CLOCK_BOOTTIME` milliseconds, meaningful only with its epoch;
only the broker evaluates proof expiry. Provers treat these absolute values as
opaque signed fields; they MUST NOT compare them with a potentially different
time namespace's clock. A deadline is expired at `now >= value` on the broker.

The challenge response MUST contain exactly these named fields (domain/version
is implicit), plus the optional unsigned `wake_error` from BROKER-018 and no
secret material. `wake_error` is outside the signed transcript. Broker fields come from stored records
and the requesting connection, including `grant_id` found by the public-key
selector in BROKER-018. Enrol proposes binding generation `1`; resume proposes
the current generation plus one. The prover MUST compare scope and key hashes
with its expected descriptor before signing. After restart, a key-selected
challenge from the authenticated broker supplies the replacement descriptor;
the child key, owner UID, parent-key hash, role, pane ID and capability hash MUST
match retained expectations. The parent key alone anchors continuity. Parent
instance/incarnation MAY change within an epoch or across epochs when the new
pending grant has the same retained parent-key hash. Those random IDs identify
scope and MUST NOT be ordered or required to be non-decreasing. Pane-generation
monotonicity applies within a `(parent_instance,pane_id)`, not across a newly
allocated parent instance. The parent-key hash comes from the parent's allocation-proven key, not a
grant argument. Verification MUST reconstruct
the bytes from stored challenge state, never trust proof-supplied replacement
fields, and compare epoch, connection and current record generation atomically
with commitment. ABP IDs and JSON member order are not signed bytes.

Allocation also requires proof of the submitted public key. `session.hello` is
a convenience read, not a gate: allocate MUST accept a valid proof whether or
not this connection called hello. Sign the concatenation: ASCII
`cosmix.native-session.allocate`, byte `00`,
u16 `1`, 16 raw broker-epoch bytes, 16 raw connection-ID bytes, 32 raw public-key
bytes, then LP of the effective policy (`default-open` if omitted). Use the
same strict Ed25519 and hexadecimal signature encoding. Verification uses this
connection's stored context and submitted key/policy before allocation. It
proves key possession, not executable identity; a captured proof cannot allocate
on another connection/epoch. Normal allocation retry rules remain BROKER-018's.

**BUS-017 — Session errors (intended).** Success is `rc: 0` with the command's
JSON result. Refusal is `rc: 10` with
`{"error_code":"INVALID_ARGUMENT","message":"...","details":{}}`.
Codes are `INVALID_ARGUMENT`, `FORBIDDEN`, `NOT_FOUND`, `STALE_GENERATION`,
`CONFLICT`, `EXPIRED`, `RESOURCE_LIMIT` and `UNSUPPORTED`; broker service failure
uses `rc: 20`, `UNAVAILABLE`. `details` is always an object and MAY contain
`field` (schema field name), `limit` (u64 decimal string), or
`retry_after_ms` (u64 decimal string), or `reason` (a command-defined token).
Consumers MUST tolerate unknown detail fields. Nonexistent and unauthorised
record/key/grant selectors MUST return the identical rc and body:
`rc: 10`, `{"error_code":"FORBIDDEN","message":"forbidden","details":{}}`.
No diagnostic may distinguish absent from another UID's resource.
The sole addition is BROKER-018's optional top-level `wake_error`, determined
only by the requester's interest quota: absent and unowned lookups under the
same quota conditions MUST still have identical errors, including that field.
Authorised state conflicts use BROKER-018's reason tokens. Proof/key errors MUST NOT echo
submitted material. Malformed requests
without a usable correlation ID MUST close the connection without mutation.
Transport loss/timeout remain local client failures, not invented peer results;
a timeout does not prove an allocation or proof failed to commit.

## Evidence and acceptance

BUS-013–017 have no implementation evidence locations or passing acceptance
results yet. The sources below support the checked legacy profile only.

Source: [wire types, parsers, validators and address tests](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/crates/cosmix-lib-bus/src/bus.rs),
[native client](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/crates/cosmix-lib-client/src/native.rs).

Acceptance must cover canonical round-trips, malformed delimiters, duplicate
and overflowing headers, JSON diagnostics, address grammar and router refusal,
transport byte caps, and command-level rejection before mutation. Existing
unit tests are evidence locations; no test run is asserted by this chapter.
