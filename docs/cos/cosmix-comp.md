# cosmix-comp

`cosmix-comp` is the Wayland compositor used by the Cosmix desktop. It can run
nested inside an existing Wayland session with `cosmix-comp --nested`, or use
the KMS backend on a system seat.

## Supported Wayland protocols

The compositor advertises the core compositor, subcompositor, seat, output,
shared-memory, DMA-BUF, explicit synchronisation, viewporter, fractional-scale,
presentation-time, XDG shell and XDG decoration globals needed by its desktop
clients.

Layer shell support is being delivered in two slices:

| Protocol | Version | Current support |
| --- | ---: | --- |
| `zwlr_layer_shell_v1` | 4 | Layer surfaces and layer popups map, are arranged through Smithay's `LayerMap`, and use the common initial-configure/ack gate. Strata ordering, keyboard focus and exclusive-zone effects on normal window placement are in progress. |

Until strata ordering lands, layer surfaces use the compositor's shared z
counter. This means a Background layer mapped after a normal toplevel appears
above that toplevel, and clicking it can raise it further. Do not rely on the
protocol layer ordering until the second slice is complete.

The first slice honours the requested output, including an explicit
`wl_output`. A request with no output uses the backend's default output; if no
output exists, the layer surface is closed and is never mapped.

## Vendored changes

The vendored Smithay layer-surface handle has one additive
`reset_after_unmap` helper so the compositor can clear Smithay's private
configure queue while applying layer-shell's protocol-mandated post-unmap
state reset.
