---
title: Bus wire format and command contracts
chapter: 4
version: 0.1.1
status: draft
date: 2026-09-05
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

## Evidence and acceptance

Source: [wire types, parsers, validators and address tests](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/crates/cosmix-lib-bus/src/bus.rs),
[native client](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/crates/cosmix-lib-client/src/native.rs).

Acceptance must cover canonical round-trips, malformed delimiters, duplicate
and overflowing headers, JSON diagnostics, address grammar and router refusal,
transport byte caps, and command-level rejection before mutation. Existing
unit tests are evidence locations; no test run is asserted by this chapter.
