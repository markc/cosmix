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

## Application layout and desktop furniture

Policy clarified by Mark on 2026-09-09: Quoin owns desktop furniture. Its
four corner-triggered edge panels are rendered inside the compositor.
Individual applications must not reproduce that furniture or embed a
Quoin-like shell around their content. The compositor manages window titles,
borders, caption buttons, movement, resizing and fullscreen presentation.

Apps use shared CTK widgets for conventional menu bars and dropdown menus,
toolbars, status rows and controls appropriate to their purpose. Application
content can include sidebars, browsers, inspectors and transport controls when
needed; adding one does not require adopting desktop panel furniture. Reuse
the widgets and behaviour without imposing the same outer layout on every app.

Media's menu bar, native file requester and unobstructed video area illustrate
this general direction; Media is not a special exception. The former mandate
that new apps use `DcsAppShell`, including the instruction to migrate Studio
when it gains a sidebar, is superseded. Existing consumers such as Tower,
FileMgr and Mail retain their current implementation until deliberately
updated; their use of `DcsAppShell` is not a template for new app furniture.

## Registry

| State | Slug | Display name | Role |
|---|---|---|---|
| active | `studio` | CosMix Studio | Recording-studio/DAW north star; drives the `musicd` domain |
| active | `filemgr` | CosMix FileMgr | Twin-pane file manager; distinct from the `filesd` domain |
| retired | `midiseq` | — | Superseded by `studio` 2026-07-24 (slug named the capability, not the destination). State roots under `cosmix/apps/midiseq` were migrated to `cosmix/apps/studio` as a one-time operator step; this slug is never reused. |
| active | `tray` | CosMix Tray | Plasma StatusNotifierItem — launch apps, start/stop cosmix daemons, mesh health (kind: tray, engine: none) |
| active | `tower` | CosMix Tower | Mesh mission control — verified node atlas, same-node citizen/daemon controls, live traffic animation, and persisted filters/layout |
| active | `mail` | CosMix Mail | Frontend mail reader and composer (Bevy + ctk); reads the `maild` domain, which stays the backend server. Not a reused retired slug: the archived Bus/`ui.*` disp-skia client of the same name was never registered here and was carved out to `_attic/bus-display/` on 2026-07-20. Landed 2026-07-31 as the widget vertical slice — fixture corpus, no JMAP transport yet. |
| active | `quoin` | CosMix Quoin | Furniture-tier desktop shell: four edge panels; Bus service `shell` |
| active | `media` | CosMix Media | Native CTK audio/video player; local MP3/MP4 playback and Bus service `media` |
| active | `term` | CosMix Term | Native Wayland Mix terminal (Bevy + ctk): rio-vt PTY/VT core, swash-rendered grid, child Mix shell per pane. P2 frontend landing (menu bar + single pane); full ABP control is P3a, gated on authenticated per-instance identity (P0-I). Not a reused slug. |
