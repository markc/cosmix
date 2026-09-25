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

Templates: `panel` (bottom panel, page `scene-panel`), `launcher`, `calendar`
and `notes`. The loader copies a template into the user's scenes directory
only on `scenes.install`; it never enables discovered directories
automatically. Installed under their own names these reuse the legacy panel
citizen's page IDs, so the loader keeps them unmounted while `quoin-panel` is
registered and mounts them when it stops. Install under a distinct name (for
example `{template:"launcher",name:"preview-launcher"}`) to try one beside the
running panel. See [the loader manual](../../docs/cos/scenes-loader.md) and
[quoin-panel](../../docs/cos/quoin-panel.md).
