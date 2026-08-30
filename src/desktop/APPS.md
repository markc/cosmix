# CosMix Desktop application registry

This is the public, append-only identity registry for installable CosMix
Desktop applications. Entries have one of three states: **active**,
**reserved**, or **retired**. Retired slugs are never reused.

## Identity scheme

| Identity | Form |
|---|---|
| Source directory | `apps/<slug>` |
| Cargo package and primary binary | `cosmix-<slug>` |
| Native application id | `dev.cosmix.<slug>` |
| Runtime state directory | `cosmix/apps/<slug>` |
| Display name | Branding only; never a storage or protocol key |

Slugs must match `^[a-z][a-z0-9]{0,15}$`.

A slug must differ from every daemon name stem. For collision checking, a
daemon `cosmix-<stem>d` reserves both `<stem>` and `<stem>d`: one-letter-`d`
pairs such as `chat`/`chatd` are banned together. Daemons are domain-named,
never app-named.

**One standing exception: `mail`.** The `mail`/`maild` pair predates this
registry and is deliberately kept (2026-07-31, Mark's call): `maild` is the
backend mail server, `mail` is the frontend reader and composer. The split was
reserved for exactly that division on 2026-05-01 and the Bus namespaces
`mail.*` and `maild.*` were designed to coexist on one hub. This exception is
closed — it grandfathers one pre-existing pair and licenses no others; a new
daemon still bars its stem.

There is one installable implementation per component. If a second rendering
engine is maintained as a comparison arm, its package and binary are
`cosmix-<slug>-<engine>`; it does not create another component identity.

## Application shell

GUI apps render their chrome through ctk's shared `DcsAppShell` (menu bar,
toolbar, DCS sidebars, centre, optional status row) — apps inject content
entities into slots and never assemble or patch shell structure. Current
consumers: Tower, FileMgr and Mail (2026-07-31). Studio deliberately stays off
the shell for now: it has no sidebar content, and its transport/song
footers are app essence, not shared chrome — don't copy its hand-assembly,
and migrate it when it gains its first side panel (sample/song browser or
channel/note inspector), not before (decision 2026-07-25). New apps must
use the shell. Contract and procedure: `_doc/2026-07-25-dcs-app-shell.md`
(control repo).

## Registry

| State | Slug | Display name | Role |
|---|---|---|---|
| active | `studio` | CosMix Studio | Recording-studio/DAW north star; drives the `musicd` domain |
| active | `filemgr` | CosMix FileMgr | Twin-pane file manager; distinct from the `filesd` domain |
| retired | `midiseq` | — | Superseded by `studio` 2026-07-24 (slug named the capability, not the destination). State roots under `cosmix/apps/midiseq` were migrated to `cosmix/apps/studio` as a one-time operator step; this slug is never reused. |
| active | `tray` | CosMix Tray | Plasma StatusNotifierItem — launch apps, start/stop cosmix daemons, mesh health (kind: tray, engine: none) |
| active | `tower` | CosMix Tower | Mesh mission control — verified node atlas, same-node citizen/daemon controls, live traffic animation, and persisted filters/layout |
| active | `mail` | CosMix Mail | Frontend mail reader and composer (Bevy + ctk); reads the `maild` domain, which stays the backend server. Not a reused retired slug: the archived Bus/`ui.*` disp-skia client of the same name was never registered here and was carved out to `_attic/bus-display/` on 2026-07-20. Landed 2026-07-31 as the widget vertical slice — fixture corpus, no JMAP transport yet. |
