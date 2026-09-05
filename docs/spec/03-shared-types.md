---
title: Shared types and validation boundaries
chapter: 3
version: 0.1.1
status: draft
date: 2026-09-05
---

# Shared types and validation boundaries

## Lexical invariants at the baseline

| Type | Accepted syntax | Construction and decoding |
| --- | --- | --- |
| `PropPath(String)` | `[a-z0-9_]+(\.[a-z0-9_]+)*`; no wildcard, empty segment, Unicode or hyphen | Private field; fallible `new`; `FromStr` and manual `Deserialize` call `new` |
| `NamespaceName(String)` | `[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*` | Private field; fallible `new`; `FromStr` and manual `Deserialize` call `new` |

Sources:
[PropPath and tests](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/crates/cosmix-lib-props-core/src/path.rs),
[NamespaceName and tests](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/crates/cosmix-lib-props-store/src/namespace.rs).
These are structs, not enums; their error types are enums. Neither defines an
explicit `TryFrom` conversion at this revision. Both retain transparent string
serialisation. Neither constructor imposes a byte-length or segment-count limit.

**TYPE-001 — One lexical invariant per type.** All safe construction and decoding
paths must preserve the type's accepted grammar. Do not derive transparent
`Deserialize` around an unchecked inner string when a fallible constructor is
the invariant gate. Keep serialisation compatible for valid values.

**TYPE-002 — Nested decoding must use the field type.** Ordinary derived Serde
decoding of a `PropDescribe.path`, its `children`, or `RecordKey.namespace` invokes
that field type's `Deserialize`. The baseline manual implementations therefore
reject invalid strings inside those enclosing records without a new generic
validation trait. This does not cover raw `String`/`Value` fields, custom decoders
that bypass the type, or environment-dependent relationships between fields.

**TYPE-003 — Distinguish selectors.** A property path is not a record namespace,
record key or structured field selector. `RecordKey` contains an unqualified
`NamespaceName`, a `String` key and `Vec<FieldPathSegment>`; the latter uses
`Field(String)` or `Index(u64)`. A constructible selector is not proof that a
particular wire operation supports field projection.

## Required environment gates

The following are responsibilities, not claims of a new implemented trait API.
Keep lexical parsing pure. Route contextual checks through the owning registry,
schema, authenticated request context and atomic mutation boundary.

| Gate | PropPath | NamespaceName / enclosing RecordKey |
| --- | --- | --- |
| Input budget | Bound transport bytes, nesting and allocation before/while decoding | Same; constructors alone allocate before rejecting oversized input |
| Lexical form | `new` / `FromStr` / `Deserialize` | `new` / `FromStr` / `Deserialize` |
| Resolution | Resolve the path in the selected daemon's property tree | Resolve the unqualified namespace in the owning service's registered namespace set |
| Operation support | Read/describe/write support; mutable/read-only property | Registered cardinality, key policy, supported verb and selector shape |
| Identity and permission | Trusted transport provenance, permitted access, redaction | Service-qualified capabilities and authenticated actor; never trust caller-supplied authority strings |
| Schema | Value type and property-specific domain constraints | Record schema, primary key and cross-field constraints; reserved/substrate fields |
| State and race safety | Revision/own-operation rules where revisioned writes apply | Current version, lifecycle and hooks; authorise and commit without stale-state acceptance |
| Durable outcome | Publish only the accepted transition | Atomic record/history effect, recovery and safe event/audit projection |

**TYPE-004 — No syntactic authorisation.** A `NamespaceName` accepts dotted names
without knowing the service. `qualified(service)` returns a composed `String`;
it neither validates nor authenticates the service argument. A lexical namespace
cannot prove that it is registered or that the caller may access it.

**TYPE-005 — Contextual validity belongs at use.** A proposed `ValidatedRequest`
or equivalent contract is warranted only where several consumers need to share a
contextual invariant and its lifetime/atomicity semantics. Adding a marker trait
does not solve a TOCTOU race. Do not add one merely to duplicate existing string
validation. [Properties](06-properties.md) owns the actual routing behaviour.

## Reconciled Actor and Capability boundaries

Commit `4d2f1ebb77af51d8bbd08cb18f4e7070cebb58ac` subsequently changed
props-store to 0.3.0. Inspection of its
[Capability changes](https://github.com/markc/cosmix/blob/4d2f1ebb77af51d8bbd08cb18f4e7070cebb58ac/src/crates/cosmix-lib-props-store/src/capability.rs)
and [Actor changes](https://github.com/markc/cosmix/blob/4d2f1ebb77af51d8bbd08cb18f4e7070cebb58ac/src/crates/cosmix-lib-props-store/src/record.rs)
shows fallible construction and manual validating deserialisation. Capability
enforces non-empty only; it deliberately remains an opaque vocabulary, not a
fixed-alphabet or authorisation proof. Actor validates its supported token forms.
The unchecked-construction finding at the original baseline is historical. All
47 changed files were reconciled against the candidate; no Rust tests were rerun.

| Type | Current accepted form and entry points | Not established by acceptance |
| --- | --- | --- |
| `Actor(String)` | Private field; fallible `new`, `from_token`, `service`, `operator`, `daemon_complete`, `FromStr` and manual Serde; `reconciliation()` returns a fixed checked token | Caller authentication or actor category merely from the helper name |
| `Capability(String)` | Private field; non-empty string through `new`, `TryFrom<&str/String>`, `FromStr` and manual Serde | Alphabet, authority or recognised grant vocabulary |

Actor service/runtime tokens are non-empty ASCII letters (either case), digits,
underscore, hyphen or dot. Generic runtime forms are `token:uuid[:seq]`, with an
8-4-4-4-12 hexadecimal UUID shape and an optional non-empty ASCII decimal sequence.
The parser does not enforce UUID version/variant, sequence width or monotonicity.
`operator:principal` accepts any non-empty remainder, including colons, whitespace,
Unicode and controls. `daemon:token` is a special form; longer daemon forms can
match the generic runtime grammar. Bare `operator` and `daemon` are service tokens.
Helpers route through the general grammar, not separate category validators.

Capability whitespace, Unicode and controls remain valid if non-empty. An empty
`CapabilitySet` is valid; decoding one empty member rejects the entire set.
Ordinary nested record/event decoding invokes these manual field decoders. Raw
strings and custom bypasses still need their own gates. Shape does not establish
the intended activity-emitter obligations in [properties](06-properties.md).

`Version`, `Nseq` and `AuditEpoch` still expose `u64` tuple fields; their `next()`
uses ordinary addition, not explicit overflow errors. These and contextual
authorisation/state checks remain separate from the completed lexical change.

**TYPE-006 — Test the boundaries, not just constructors.** Require direct invalid
decoding, invalid nested fields/collections, valid round-trips and supported
ingestion-path tests. Baseline PropPath/NamespaceName tests exist; this audit did
not execute them. Add contextual rejection/race tests separately where behaviour
depends on registration, capabilities, schema or current state.
