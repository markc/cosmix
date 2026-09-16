# cosmix-scene

`cosmix-scene` parses a bounded Mix Scenes v0 document into a renderer-neutral
tree. A document is a Bus envelope with required `scene: 1`, `name` and
`citizen` headers. `window`, `subscribe`, `targets` and `model` are optional
single-line JSON headers. `window.kind` is `edge` in v0; the envelope is the
mount request and a window-family node is optional. If both exist, their kinds
must agree.

The body contains exactly one fenced `mix` strict-data map. Nodes have the
shape `id: { widget: family, ...ports }`. The ten families are `window`,
`column`, `row`, `text`, `field`, `button`, `toggle`, `list`, `image` and
`spacer`. Their required and optional ports, defaults and handler ports are
described by `describe()`. Enum ports (`window.kind`, `window.edge`,
`row.align`, and `button.tone`) are validated during lint. Numeric ports are
always JSON floating-point values: `13` resolves and travels as `13.0`.

Lists require rows of `{id: string, cells: [string]}`, a sibling `row`
template and a positive `row_height`. Template subtrees may contain only row,
column, text, spacer and image. `{cells[i]}` is allowed only in `text.text`,
and must be within the minimum cell count across all rows. Template nodes are
marked in `ResolvedScene`; their ids are not rendered. `@` is reserved for
future `<template>@<row>` instance ids and is rejected in source node ids.

Lint reports bounded-document, schema, graph, template, row and header
diagnostics. `orphan-node` is a warning; other violations are errors. The
resolver returns diagnostics for unknown families rather than panicking.

`diff(old, new)` emits `Remove`, `Insert { parent, index }`, `SetPort`,
`Reparent`, and scene-level `SetScene` operations for name, citizen, window
and subscribe changes. A dropped port emits `SetPort` with JSON `null` to
clear it. Consumers apply operations in this order: Remove, Insert, SetPort,
Reparent. Scene operations should update the mount metadata as part of the
same reload.
