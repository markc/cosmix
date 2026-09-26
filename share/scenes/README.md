# Shipped scene templates

Canonical, read-only template directories live here:

```
<template>/scene.mix
<template>/template.conf.mix
<template>/behaviour.mix       (optional executable citizen)
```

`scene.mix` is an AMP scene document, not an executable script.
`template.conf.mix` is a strict-data map. Only `behaviour.mix` executes.
Reusable behaviour helpers are shipped in `lib/` (`data.mix`, `models.mix`,
`runtime.mix`, `taskbar.mix`).

Templates: `panel` (bottom panel, page `scene-panel`), `launcher`, `calendar`,
`notes` and `settings`. The loader copies a template into the user's scenes directory
only on `scenes.install`; it never enables discovered directories
automatically. Installed under their own names these reuse the legacy panel
citizen's page IDs, so the loader keeps them unmounted while `quoin-panel` is
registered and mounts them when it stops. Install under a distinct name (for
example `{template:"launcher",name:"preview-launcher"}`) to try one beside the
running panel. See [the loader manual](../../docs/cos/scenes-loader.md) and
[quoin-panel](../../docs/cos/quoin-panel.md).

`settings/` is Settings/Appearance. It authors the scene `quoin-settings` on
Quoin's declared page `settings.appearance`, so `{template:"settings"}`
installs under that name and keeps the page. Its behaviour does not own any
state: it reads `shell.settings.get`, wakes on `shell.settings.changed` and
forwards clicks to Quoin's `shell.settings.*` verbs. Quoin also builds its
built-in fallback page from this `scene.mix` at compile time, so edit the
node block here and nowhere else. The template mounts on the right edge; edit
`window.edge` if your configuration declares the page elsewhere.

## Gallery metadata

`template.conf.mix` may carry these optional keys, which the
[Scene Editor](../../docs/cos/scene-editor.md) gallery reads through
`scenes.templates`:

| Key | Type | Meaning |
| --- | --- | --- |
| `title` | string | gallery title (defaults to the scene name) |
| `description` | string, at most 200 characters | the one-line gallery text |
| `recommended` | bool | part of the Recommended set |
| `order` | int | gallery order (then template name) |
| `requires` | list of Bus service names | shown as "needs …" while not registered |
| `hidden` | bool | not listed by `scenes.templates` |

The five shipped templates are the Recommended set: `panel` (order 10,
requires `comp` and `apps`), `launcher` (20, `apps`), `calendar` (30),
`notes` (40, `notify`) and `settings` (50).

## The editor template

`editor/` is the Scene Editor itself. It has `hidden:true`, so the gallery
never lists it. Its `scene.mix` header is a dialog:
`{"kind":"dialog","w":880,"h":620,"title":"Scene Editor","chrome":true}`.
If a host cannot show dialogs, change it to
`{"kind":"edge","edge":"right","w":480,"title":"Scene Editor"}`; the loader
opens that as an edge popup. Its helpers (the model builder, click planning
and the ced diagnostics diff) live in `editor/lib.mix`, beside the
behaviour, which loads them with `require($SCENE_DIR .. "/lib.mix")`. A user
copy therefore edits its own logic. From the shared `lib/` it uses only
`runtime.mix`.

The loader runs this directory in place as safe mode and never writes to it.
`scenes.install {template:"editor"}` makes the editable user copy; see the
Scene Editor manual for how the two are chosen. No other template may use
`window.kind:"dialog"` or the page `scene-editor`.
