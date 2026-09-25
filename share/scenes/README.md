# Shipped scene templates

Canonical, read-only template directories live here:

```
<template>/scene.mix
<template>/template.conf.mix
<template>/behaviour.mix       (optional executable citizen)
```

`scene.mix` is an AMP scene document, not an executable script.
`template.conf.mix` is a strict-data map. Only `behaviour.mix` executes.
Reusable behaviour helpers may be shipped alongside these files.

The loader copies a template into the user's scenes directory only on
`scenes.install`; it never enables discovered directories automatically.
Stage A installations must use distinct names/page IDs, for example
`{template:"launcher",name:"preview-launcher"}`. The existing panel citizen
keeps its pages until Stage B. See [the loader manual](../../docs/cos/scenes-loader.md).
