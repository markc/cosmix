---
title: Pinned specification releases
version: 0.1.0
status: implemented
---

# Pinned specification releases

Noded 0.14 adds an opt-in public-spec adapter. Serving a draft is not accepting
its requirements or promoting it to project authority. Editorial chapter order
does not reassign legacy numeric identities.

## Configuration and lifecycle

Set both `COSMIX_SPEC_RELEASE_DIR` and `COSMIX_SPEC_RELEASE_SHA256` before starting
noded. The latter is the lowercase SHA-256 of the exact `manifest.json` bytes,
provided through trusted operator configuration. Neither a digest inside the
manifest nor a release's self-declared name establishes trust.

The root contains `manifest.json` and explicitly listed Markdown files. Noded
validates and preloads the complete release before broker readiness. A missing,
malformed or mismatched configured release fails startup; it never falls back
to legacy `_spec` discovery. Neither variable set preserves the existing legacy
directory behaviour. Setting only one is an error.

The directory and its ancestors must be operator-controlled and unavailable to
untrusted writers. Leaf symlinks, special files, executable files and path
components in document filenames are rejected. These checks are not a sandbox
against concurrent malicious filesystem writers. Contents are digest-checked,
UTF-8 decoded and cached; later file edits cannot change a running snapshot.
Updates and rollback require an explicitly pinned release and broker restart.

Limits: 1 MiB manifest, 1 MiB per document, 16 MiB combined source bytes,
128 documents, 128 legacy entries, 32 related documents per legacy entry.
The manifest byte cap bounds deserialisation input; entry limits are checked
after bounded JSON parsing, not before every individual allocation.

## Manifest schema 1

Top-level fields: `schema_version: 1`, `release_id`, `status: "preparation-only"`,
`source_commit` (40 lowercase hexadecimal characters), `documents` and `legacy`.
Unknown fields are rejected. Release and document IDs are 1–96 ASCII
alphanumeric/hyphen characters, beginning with an alphanumeric character.

Each document has unique `id`, unique `file` (a single `.md` filename with the
same stem grammar), and a lowercase `sha256` digest of its complete raw bytes.
Each legacy record has unique `id` (two digits with an optional lowercase
supplement letter), nonempty `subject`, `disposition`, and an ordered
`related_documents` list of distinct existing document IDs.

The supported dispositions are `unavailable`, `tombstone` and `reserved`.
Reserved records have no related documents. Schema 1 deliberately does not
support `serve`: related documents are reading references, not approved full
legacy payloads. An operator-approved manifest determines which identities
exist and their dispositions; noded does not invent mappings from filenames.

## Retrieval in release mode

`spec.get` accepts exactly one selector:

- `name`: registered case-sensitive filename, with optional `.md`. Successful
  responses retain the legacy prose body and scalar metadata headers. Only
  `title`, `chapter`, `version`, `status` and `date` are copied. Transport command,
  rc, routing, framing and correlation headers cannot be supplied by frontmatter.
- `chapter`: integer, integer-valued finite float, or ASCII digit-only decimal
  string in 0..4294967295. Negative, fractional, overflowing and malformed values
  are rejected; no truncation or narrowing. Numeric lookup preserves old IDs.

Missing, conflicting and unknown selectors return rc 10. Numeric requests return
rc 10 and an `error`/`code` JSON body: `legacy_unavailable`, `legacy_tombstone`,
`legacy_reserved` or `legacy_unknown`. This is not a successful pointer response.
The native client's `call_typed` surfaces these as application errors, not
transport failures; its older `call` wrapper still collapses either into `Err`.

`spec.v2.get` accepts exactly one string selector, `document` or `legacy`.
Success is JSON with `schema_version`, `release_id` and `result`:

- A document result contains `document` (its manifest record) and `raw_markdown`,
  preserving full frontmatter and body bytes after UTF-8 decoding.
- A legacy result contains `legacy` (its record), ordered `documents` in that
  same document-result form, and `legacy_equivalent: false`.

Unknown targets and invalid selectors are explicit rc-10 errors. With release
mode disabled, `spec.v2.get` returns `release_unavailable`.

Encoded v2 bodies exceeding the shared 8 MiB frame ceiling minus 64 KiB header
headroom return `response_too_large`. Fetch documents individually when a bundle
does not fit. This check includes JSON escaping, not just source Markdown size.
Legacy plain-body replies retain the existing Bus parser's trailing-whitespace
trimming; v2 preserves those bytes inside the JSON `raw_markdown` string.

No legacy retained topics are seeded in release mode because no `serve` payloads
exist. V2 is initially pull-only. Before a deployment, verify that old retained
values and external caches will not present mixed-generation content; restart
alone is not evidence of clearing copies on other brokers or subscribers.

## Verification scope

The `spec_release` tests exercise bounded loading, hashes, manifest structure,
selectors, legacy identity, metadata filtering, unlisted-file exclusion,
symlink/executable rejection, file edits, restart and explicit rollback.
The noded dispatcher fixture checks response correlation, command, rc and body.
These tests do not attest a fleet deployment or authority cutover.
