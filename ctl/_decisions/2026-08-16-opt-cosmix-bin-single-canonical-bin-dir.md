# /opt/cosmix/bin is the single canonical bin dir — desktop dist retired

**Decision (Mark, 2026-08-16):** all cosmix binaries live in `/opt/cosmix/bin`
— daemons, `mix`, GUI apps, and the desk-CT desktop trio alike. No parallel
bin dirs.

This revises decision D2 of the comp→desktop arc (same day, journal
`_journal/2026-08-16-comp-desktop-v0-v2.md`), which staged the desk trio in
`/var/lib/cosmix/desktop-dist` and bound it into desk CTs at
`/opt/cosmix-desktop`. Mark saw the split and directed the merge: the staging
dir and the second mount name are **retired**.

## What changes

- The desk binaries are refreshed directly in `/opt/cosmix/bin` by
  `_bin/desk_bin_refresh.mix` (renamed from `desk_dist_refresh.mix`; same
  atomic + sha-gated + observed-set-stability logic, now scoped to named
  binaries inside a shared dir — the desk pair comp + quoin-demo since the
  single-owner ruling below; filemgr is deploy_desktop's).
- Desk instances bind the host's `/opt/cosmix/bin` read-only at the **same
  path** (`BindReadOnly=/opt/cosmix/bin:/opt/cosmix/bin`), shadowing the CT
  image's own `/opt/cosmix/bin` (factory mesh set: maild/noded/webd/mix — all
  masked in desk CTs). A desk CT therefore rides the host's `mix` and sees
  exactly the host's canonical toolset.
- Only `bin` is bound, not all of `/opt/cosmix` — the host's `vhosts/`,
  `share/`, etc. stay host-side.

## What survives

- **Decision D1 stands:** the desk template stays binary-free; instances get
  binaries via the RO bind, never baked in.
- `deploy_desktop.mix` remains the identity-canon deployer for GUI apps
  (slug-derived desktop entries/icons); `desk_bin_refresh.mix` only syncs its
  named binaries from the cos desktop release target and never touches
  anything else in the dir.

## Single owner per name: `cosmix-filemgr` is deploy_desktop's (RULED)

The first cut of this migration left `cosmix-filemgr` with two uncoordinated
publishers: `desk_bin_refresh.mix` (trio set) and `deploy_desktop.mix`
(RUST_SLUGS), under non-intersecting locks, with `install -T` writing in
place and different `-p` feature-unification — last writer silently won
(the 2026-08-16 refresh replaced a stale Aug-6 deploy_desktop copy; the
converged review flagged the overlap as an open MAJOR).

**Ruling (Mark, 2026-08-16): option B — one deployer per name.**
`deploy_desktop.mix` owns `cosmix-filemgr` (whole identity: binary, desktop
entry, icon, manifest ledger, `--only filemgr` selective deploys);
`desk_bin_refresh.mix` drops to the desk pair (`cosmix-comp`,
`cosmix-quoin-demo`), whose canonical build is one cargo invocation:
`cargo build --release -p cosmix-comp --features cosmix-comp/kms-live
-p cosmix-quoin-demo`. Desk CTs ride deploy_desktop's filemgr through the
same RO bind — comp and filemgr talk Wayland across a socket, so
cross-build client/compositor is the normal case, not a coherence break.

**Rider (same ruling): deploy_desktop's BINARY installs publish
atomically** — stage to a dot-tmp in `$BIN_DEST`, then `mv -fT`
(rename(2)): the bound dir keeps the never-torn guarantee, and a running
host app or desk-CT client can no longer ETXTBSY a deploy. Its
config/desktop/icon installs stay plain `install -T`; nothing exec()s
those mid-write.

**Within-owner invariance (review finding, closed by enforcement):** naming
one owner did not by itself make that owner's bytes deterministic —
`--only filemgr` built `-p cosmix-filemgr` alone, a different
feature-unification set from the full five-package deploy (cargo-tree
verified: the shared CTK dep gains `mixer`, lib-config gains
client-helpers, zbus gains blocking-api in the full set), so the same
deployer installed different filemgr bytes depending on invocation shape.
`deploy_desktop.mix` now passes the full Rust `-p` set on every build
regardless of `--only` selection and installs only what was selected —
installed bytes are invocation-invariant. `--no-build` remains outside
the guarantee by definition (it trusts whatever sits in target/release).
The ownership invariant itself is also enforced, not remembered, and in
BOTH directions: `desk_bin_refresh.mix` refuses at preflight to publish
any name deploy_desktop's live manifest claims — installed entries and
the shared ledger's preclaimed `.cosmix-<slug>.new` dot-tmps alike, so a
slug deploy_desktop declares but has never installed on this host still
counts as claimed; and `deploy_desktop.mix` refuses at parse time any
`$RUST_SLUGS` entry in the refresher's declared pair (comp, quoin-demo),
which is the direction the live ledger cannot see until after the
overwrite has shipped. Residual: a name added to both deployers is
invisible only on a host where deploy_desktop has never run at all AND
the static pair list was not updated — two simultaneous omissions, each
of which alone is caught.

**Ownership transfer procedure** (the guards refuse until every step is
done — the manifest ledger never self-prunes, and a published name holds
TWO claim kinds: the `installed[]` entry and the shared ledger's
preclaimed `.cosmix-<slug>.new` dot-tmp):

1. Decide the direction and update ALL static declarations in the same
   commit. Refresher → deploy_desktop: remove the name from `$NAMES`
   and `$REFRESHER_OWNED_SLUGS`, add its slug to `$RUST_SLUGS`.
   deploy_desktop → refresher needs THREE edits, none optional: move
   the slug `$RUST_SLUGS` → `$RETIRED_SLUGS` (never delete — the
   lost-manifest orphan scan must keep probing its desktop/unit/icon
   residue, still this deployer's since the refresher is binary-only),
   add it to `$REFRESHER_OWNED_SLUGS`, and add the binary name to
   `$NAMES` in desk_bin_refresh. The `$REFRESHER_OWNED_SLUGS` entry is
   what makes the scan skip the two bin-dir names — without it a
   refresher crash-temp (same `.cosmix-<slug>.new` path) could be
   claimed into the never-pruning `shared` by a lost-manifest recovery,
   permanently re-blocking the completed handover; and the parse guard
   only refuses `$RUST_SLUGS ∩ $REFRESHER_OWNED_SLUGS`, so a `$NAMES`
   entry with no matching `$REFRESHER_OWNED_SLUGS` entry is exactly the
   drift the mirror cannot catch.
2. Edit `/opt/cosmix/share/cosmix-desktop.manifest` as root: remove the
   name's `installed[]` entry ONLY if the new owner takes over every
   file it ledgers (binary, desktop entry, icons, unit) — otherwise the
   non-binary files become unnameable, because deploy_desktop never
   deletes; and remove the `.cosmix-<slug>.new` line from `shared`.
3. Run the new owner once and confirm the old owner's next run refuses
   the name (the guards are the proof the transfer is complete).

The refresher's names today (comp, quoin-demo) are binary-only, so step
2's ledger caveat bites only if a full app (desktop entry + icons) ever
moves to a binary-only deployer — that split is itself a design smell to
stop and think about.

## Trade-offs accepted

- Host binary upgrades propagate to desk CTs instantly (running processes
  keep their old inode until restarted — unchanged from the dist scheme).
- The CT's image-pinned `mix` is shadowed while the bind is active; host-built
  binaries running under the Arch CT userland is already proven (in-CT
  `--kms-probe` on the bound comp, fleet-wide `mix` deploys).
- **Security: the RO bind is not a boundary against desk-CT root.** With
  `PrivateUsers=off` (D3's deliberate call — DRM/VT ioctls do init-userns
  `capable()` checks) the CT's root holds CAP_SYS_ADMIN in the init userns,
  and `mount -o remount,rw,bind /opt/cosmix/bin` succeeds — **live-proven on
  desk1 2026-08-16** (remounted rw, verified, restored ro; no writes made).
  A compromised desk-CT root can therefore tamper with every canonical host
  binary, `mix` (root's login shell) included. Under the dist scheme the
  same remount only exposed the disposable trio staging dir — this decision
  raises the stakes, it did not create the hole. No clean containment
  exists inside D3: dropping CAP_SYS_ADMIN breaks `Boot=yes` (systemd must
  mount API filesystems) and userns mount-locking is what D3 traded away.
  **Accepted for desk1 as a local, Mark-only experimental instance; a desk
  CT must never run untrusted workloads while this bind + PrivateUsers=off
  combination stands. Revisit at V-3.**
