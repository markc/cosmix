# cosmix-lib-files

`cosmix-lib-files` is the daemon-independent core for files-as-truth Markdown corpora and allowlisted live-filesystem operations. In the `bus <- mix <- cos` dependency chain it belongs to the `cos` layer: it uses `cosmix-lib-bus` directly, with that dependency's default features disabled, and does not depend on `mix`. The Rust library target is imported as `cosmix_files`.

## Synopsis

The crate provides two related capabilities:

- Markdown corpus primitives: frontmatter handling, atomic writes, identity, hashing, link extraction, index schema, ingest, persistence, and reconciliation.
- A stateless file-manager backend over configured places, with path containment and per-place write policy.

The default build contains the pure and filesystem-facing logic. SQLite persistence, ingest, and the complete reconcile pass are opt-in.

This crate is a library. It does not contain a CLI, a Bus service loop, a filesystem watcher, or CAS integration.

## Cargo features

| Feature | Default | Effect |
|---|---:|---|
| `sqlite` | No | Adds `rusqlite` and exposes the `store`, `ingest`, and `reconcile` modules. |

An empty default feature set keeps the core mesh-free and permits:

```console
cargo test --no-default-features
```

## Modules

| Module | Main API | Purpose |
|---|---|---|
| `atomic` | `write_atomic` | Replaces or creates a file through a unique sibling temporary file, file flush, rename, and best-effort directory flush. |
| `frontmatter` | `Parsed`, `read`, `get_field`, `set_field`, `ensure_id`, `raw_header`, `headers_to_json` | Reads canonical fenced headers and performs byte-preserving field edits. |
| `hash` | `content_hash` | Returns lowercase, 64-character BLAKE3 content hashes. |
| `id` | `new_id`, `is_valid_id` | Mints lowercase UUIDv7 identifiers and validates UUID strings. |
| `links` | `Link`, `LinkKind`, `extract_links` | Extracts unresolved wiki and Markdown document links. |
| `diff` | `Entry`, `Change`, `diff` | Classifies differences between a disk scan and an index snapshot. |
| `schema` | `APPLICATION_ID`, `USER_VERSION`, `INDEX_SCHEMA_V1` | Defines the rebuildable SQLite index schema and its identity constants. |
| `fsops` | `Place`, `FsLayer`, `resolve_within` | Provides scoped live-filesystem reads and mutations. |
| `error` | `FilesError`, `Result` | Defines the shared error surface. |
| `store` | `Store`, `DocRecord`, `ChangeRow`, `ChangeKind`, `CorpusStats` | Implements the SQLite projection and change stream. Requires `sqlite`. |
| `ingest` | `scan` | Projects one corpus file into a `DocRecord`. Requires `sqlite`. |
| `reconcile` | `ReconcileReport`, `reconcile` | Walks a corpus, computes changes, and applies them to a `Store`. Requires `sqlite`. |

`FilesError` and `Result` are re-exported at the crate root.

## Frontmatter

The frontmatter grammar is a `---`-fenced, flat `Key: value` header. Values beginning with `[` or `{` are interpreted as JSON for the index projection; other values remain strings. Reads delegate to the canonical lenient Bus parser.

`frontmatter::read` returns:

- key-sorted parsed headers;
- the parsed body;
- non-fatal warnings for non-canonical, skipped, or invalid-JSON lines.

`frontmatter::set_field` edits raw text rather than serialising a parsed header. It replaces only the selected value line, appends a missing field before the closing fence, or creates a minimal header when none exists. Untouched field order, comments, list lines, blank lines, quoting, JSON text, line endings, and body bytes remain unchanged.

Values passed to `set_field` must be single-line. Existing field separators are normalised to `: ` on the edited line.

`frontmatter::ensure_id` preserves an existing valid identifier. If the field is absent or invalid, it inserts a new UUIDv7 and returns the updated content, identifier, and a flag indicating that minting occurred.

## Identity, hashing, and links

Document identity is an opaque UUID. `id::new_id` uses UUIDv7 so generated identifiers are time ordered and lexicographically sortable. `id::is_valid_id` accepts any UUID version.

`hash::content_hash` hashes the complete file bytes with BLAKE3.

`links::extract_links` recognises:

- `[[target]]` wiki links;
- `[text](target)` Markdown links.

Wiki aliases and anchors are removed from the extracted target. Optional Markdown link titles are dropped. Image links are ignored. Targets remain unresolved; index-aware resolution is outside this module.

## Reconcile classification

`diff::diff` compares `Entry` values from disk with the current index snapshot.

| `Change` | Meaning |
|---|---|
| `Added` | A managed file appears at a new path. |
| `Changed` | A known path has changed content, identity, or keeper role. |
| `Removed` | An indexed path is absent from disk and was not consumed by a rename. |
| `Renamed` | A new path carries the identity of an indexed keeper whose old path disappeared. |
| `DuplicateId` | More than one live path carries the same identity. |
| `Unmanaged` | A file has no valid identity. |

For duplicate identities, the lexicographically lowest path is the deterministic keeper. Other paths are reported as duplicates rather than rejected or allowed to overwrite the keeper.

Passive scans do not mint identifiers. An id-less file remains unmanaged until an explicit authoring path adopts it.

## SQLite projection

The `sqlite` feature exposes one rebuildable index per corpus. Markdown files remain authoritative; the database is a derived projection keyed by relative path. Document identifiers are indexed but intentionally not unique.

The v1 schema contains:

- document projections and tombstones;
- a per-corpus monotonic modification sequence;
- an ordered change stream;
- a derived link graph;
- attachment references;
- conflict-file sightings;
- an FTS5 document table.

`Store::open` creates or validates an on-disk index. `Store::open_in_memory` provides the same schema in memory. A database is bound to one corpus identifier.

Read methods expose the current modification sequence, live reconcile entries, changes since a cursor, corpus statistics, live and tombstoned paths, document metadata, and metadata substring search.

Mutation methods are transactional:

- `upsert` adds, resurrects, or updates a path and emits `ADDED` or `CHANGED`;
- `tombstone` soft-deletes a live path and emits `REMOVED`;
- `rename` preserves the document row identity and emits `RENAMED`;
- `wipe` clears the rebuildable projection and resets the modification sequence.

Callers supply `now_ms` to mutation and reconcile methods. The store does not read the clock for those records.

`ingest::scan` reads one file, hashes its complete bytes, parses frontmatter, projects common fields, records size and modification time, counts body words, and marks an invalid or absent identifier as unmanaged.

`reconcile::reconcile` walks Markdown files without following symlinks. It skips dotfiles, dot-directories, editor temporary files, and conflict sidecars as documents. It scans, diffs, applies index changes, records current conflict sidecars, removes resolved conflict records, and returns per-class counts plus per-file errors.

Reconciliation reads corpus files but does not rewrite them or mint identifiers.

## Live-filesystem layer

`Place` defines a bounded filesystem root and its presentation and policy fields:

- `id`, `label`, `group`, `icon`, and `order`;
- `root`;
- `writable`;
- optional `allow` and `deny` path patterns.

An `FsLayer` contains an immutable places list and a trash root. Paths use the form `<place-id>/<relative-path>`. Reads may address a place root; mutations of the place root are refused.

`resolve_within` rejects absolute paths, empty paths when disallowed, `.` and `..` components, and paths whose deepest existing ancestor resolves outside the configured root.

Allow and deny patterns are slash-separated. `*` matches within one path segment. Deny rules take precedence. An allowed path's ancestors may be traversed for listing but are not themselves writable targets. Policy-scoped places also reject symlink, hardlink, and non-regular-file paths where those forms could cross the policy boundary.

Read operations are:

| Method | Result |
|---|---|
| `places` | Place metadata for a sidebar or equivalent client. |
| `list` | A live directory listing with metadata, counts, sorting, and hidden-file control. |
| `stat` | File or directory metadata. |
| `read_blob` | A capped UTF-8 preview or a binary marker. |
| `search` | Case-insensitive filename substring search. |
| `tree` | A depth- and node-capped folders-only tree. |

Mutation operations are:

| Method | Behaviour |
|---|---|
| `mkdir` | Creates one directory or its parent chain. |
| `touch` | Creates a new empty file without clobbering. |
| `write` | Writes bytes atomically, with explicit overwrite control. |
| `copy` | Reads from one place and writes to another; only the destination must be writable. |
| `move_` | Moves a path; both source and destination places must be writable. |
| `delete` | Deletes a file or, when explicitly recursive, a directory. Missing targets succeed. |
| `trash` | Moves a path to reversible trash and records its scoped origin. |
| `trash_list` | Lists trash metadata. |
| `trash_restore` | Restores to the revalidated writable origin without clobbering. |
| `trash_empty` | Permanently removes all trash entries. |

Copy and move reject a destination that is the source or lies inside it. Cross-filesystem moves fall back to copy followed by removal. Atomic writes preserve existing destination permission bits on a best-effort basis.

## Dependencies

The always-on dependencies are `cosmix-lib-bus`, `blake3`, `uuid`, `serde`, `serde_json`, and `thiserror`. `rusqlite` is optional and enabled only by `sqlite`.
