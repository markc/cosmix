# cosmix-stored

The offsite store names immutable snapshots over blobd's bytes. Its control
surface is a mesh-open Mix citizen; file bytes use blobd's discovered lane.

## Manifest v2

The only fields are `schema_version: 2` and `files`, an array of objects with
exactly `blob`, `path`, `size`. Blob IDs are lowercase `b3:<64 hex>`. Sizes and
their sum are exact nonnegative integers through 9007199254740991. One blob
cannot have conflicting sizes. Empty files and empty snapshots are valid.

`mix/store_manifest.mix` beside blobd is the shared canonicaliser. It emits
compact UTF-8 JSON, lexical field order (`files`, `schema_version`; `blob`,
`path`, `size`), files sorted lexically by path, no trailing newline. String
scalars use JSON escaping; identity is BLAKE3 of those exact encoded bytes.
Origin, instance, creation time and collection do not participate in identity.

Limits: 1000 files, 128 KiB canonical UTF-8 bytes, 1024 UTF-8 bytes per path.
Paths reject Unicode control characters, backslashes, colons, empty components,
absolute paths, `.`/`..`, duplicates and file/directory prefix collisions.
No MIME, times, permissions, xattrs, links or empty directory preservation.
Collection names match `[A-Za-z0-9_-]{1,64}`; owners are `store:<collection>`.

## Old pilot identity

Migration first verifies SHA-256 of the **original raw manifest bytes**, then
validates schema 1 and requires exact canonical-byte equality. The pilot's
file-array order is significant and MUST NOT be sorted before verification.
Only afterwards may a v2 manifest be constructed. Rewritten manifests must
pass the new byte limit; migration must never truncate or omit entries.

## Tests

From the checkout, without root or a broker:

```text
mix src/crates/cosmix-blobd/mix/store_manifest_test.mix
```
