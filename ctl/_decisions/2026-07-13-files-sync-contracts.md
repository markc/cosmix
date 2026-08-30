# Files + sync semantics contracts (cosmix-cloud oneshot WS4)

**Status:** contracts doc, 2026-07-13. The load-bearing WS4 deliverable. Decisions
are Codex's (design consult D7, thread 019f56f0); this doc makes them normative so a
later session builds against a fixed target. Supersedes the abandoned "syncd
wraps syncthing with an ABP surface" design for the canonical-store and
multi-writer questions. Historical record:
`git show "$(git rev-list -1 HEAD -- _plan/2026-05-06-syncd.md)^:_plan/2026-05-06-syncd.md"`.

## The decision in one line

**Do NOT build `cosmix-syncd`. One canonical byte-store per account under filesd;
Syncthing (systemd-owned) syncs that same tree; the only new code is a webd
public-share module** (54 live NC public links are the real dependency, not a sync
daemon).

## C1 — Canonical store (who owns the bytes)

- **One filesd-controlled root per node, one jailed subtree per stable account id.**
  The bytes live **once**. Syncthing is pointed at that same tree — it does not own a
  second copy, and filesd does not shell a copy into a syncthing spool.
- Path shape: `<filesd-root>/<account-id>/…` where `<account-id>` is the stable maild
  account id (never the email — emails change). filesd's existing `FsLayer` jail
  (`is_safe_rel`, `.Trash` handling in `cosmix-lib-files/fsops.rs`) is the enforcement
  point; a share/sync root is just another configured place.
- Rationale: a single source of truth is the only way the four writers below stay
  consistent; two copies with a sync step is exactly the Nextcloud-client failure mode
  (serial re-copy) this whole effort is leaving.

## C2 — Multi-writer semantics (syncthing + web-upload + media-index + shares)

Four things touch the same bytes. The contract:

1. **All web mutations go through filesd** using temp-file + atomic-rename
   (`cosmix-lib-files/atomic.rs` already provides this). A partially-written upload is
   never visible; syncthing never sees a torn file mid-write.
2. **Syncthing is the second writer.** It writes via its own rename-into-place; filesd
   treats externally-appearing files as first-class (it already stats the live tree, it
   holds no authoritative file index that could go stale).
3. **Media indexing is strictly read-only** (WS3's indexer opens the DB `rw` but the
   media tree read-only) and **retries** any file whose size/mtime changes between stat
   and read (a syncthing write landing mid-index). It never blocks a writer.
4. **Namespace/metadata is eventually consistent** — the media index, share catalogue,
   and any future search index are *derived* views that reconcile on rescan, never a
   lock on the byte-store.

No global lock. Correctness comes from atomic-rename (no torn reads) + read-only
derived indexes (no index-vs-bytes divergence that can corrupt), not from serializing
the writers.

## C3 — Conflict handling

- Syncthing's conflict siblings (`*.sync-conflict-<date>-<device>.<ext>`) are
  **preserved, indexed, and surfaced** to the user — never auto-resolved.
- Resolution is **explicit**: keep-current / keep-conflict / keep-both. A future
  filesd verb (`fs.conflict.{list,resolve}`) or the share/UI layer drives it; until
  then conflicts are visible as ordinary files (the honest default — nothing silently
  picks a winner).

## C4 — Trash + restore (recovery, NOT backup)

- **Trash:** filesd already implements freedesktop XDG-Trash (`.Trash/{files,info}`,
  `TrashInfo` with `DeletionDate`, restore-by-token — `fsops.rs` `trash`/`trash_list`/
  `trash_restore`/`trash_empty`). **This is the trash authority.** A web delete
  **archives to trash first**, never an in-place unlink.
- **Versions:** Syncthing's `.stversions` is the version archive (staggered
  versioning). A filesd **restore catalogue** (SQLite: path, version-file, timestamp)
  indexes `.stversions` so restore-after-accidental-overwrite is a lookup, not a
  filesystem hunt. Retention policy lives with the catalogue.
- **Explicitly not backup.** Trash + `.stversions` are convenience recovery. Filesystem
  snapshots (the CT is on a ZFS/btrfs mount; PBS covers it — P6 set `backup=1` on the
  500GB mp0) remain the real backup + ransomware defence. This must be said out loud:
  a synced delete propagates; only snapshots survive a malicious mass-delete.

## C5 — Permissions model

- Dedicated service ownership; **`0750` directories, `0640` files**; account-root
  isolation (a jail per account id — filesd's existing path-jail enforces it).
- **No symlink escapes** (filesd's `is_safe_rel` + no-follow on the jail boundary).
- Per-root Syncthing **device ACLs** (which mesh devices sync which root) — belt over
  WG's network trust.
- **Public access ONLY through explicit, revocable tokens** (the share module, C6) —
  never by exposing a filesystem path.

## C6 — Public shares (the only new build) — webd module

Replaces NC public links (54 live). Scaffolded this run as `cosmix-webd`'s
`file_share` module — SQLite catalogue + create/list/revoke + jailed serve.

- **Schema** (`file_shares` in the per-vhost or a dedicated shares DB):
  `id` (opaque token, ≥128-bit base64url), `account_id`, `rel_path` (jailed, relative
  to the account root), `kind` (`file`|`dir`|`drop`), `password_hash` (nullable, bcrypt),
  `expires_at` (nullable unix), `created_at`, `revoked` (0/1), `download_count`.
- **ABP/handler surface:** `share.create {account, rel_path, kind, password?, expires?}`
  → token; `share.list {account}`; `share.revoke {token}`. Authored from a logged-in,
  CSRF-gated, authz-checked webd context (the account must own `rel_path`).
- **Serve:** `GET/HEAD /s/<token>` → resolve token (not revoked, not expired, password
  ok) → **jailed** read of `account-root/<rel_path>` (the same `is_safe_rel` jail;
  token never carries a filesystem path, only the catalogue row does) → stream with
  Range support. A revoked/expired/bad-password token is indistinguishable (404/401),
  no path leak.
- **File-drop** (outsiders upload into a `kind=drop` share) and Thunderbird FileLink
  are the same catalogue + an upload route — deferred past this run, noted in the
  scaffold's gap list.

## C7 — What is NOT built (honest gap list)

- **No `cosmix-syncd`.** Syncthing runs under systemd, hand-configured with
  global discovery, local discovery, relaying, NAT traversal, usage reporting,
  and auto-upgrade disabled. Its GUI/API is loopback-only and its sync listener
  is bound to the node's WireGuard address. A thin Mix event bridge (Syncthing
  REST events → ABP topics) is optional and unbuilt.
- **No versioning verbs yet** — `.stversions` exists once syncthing runs; the filesd
  restore catalogue is speced here, not coded this run.
- **No conflict-resolution verb** — conflicts surface as files; the resolve verb is
  future.
- **The share module is a compiling scaffold**, not a routed production surface — see
  its own real-vs-stub note.

## Migration note

"syncthing retires files sync" is true ONLY once these contracts survive a
migration + conflict + restore test with Mark's real 212GB. This run builds the
contracts + the share scaffold; the sync cutover is a later, evidence-gated session.
gco NC stays the files authority until then.
