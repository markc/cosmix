# cosmix-disp-skia — the desktop surface

`cosmix-disp-skia` is the reference renderer for the Bus Display Protocol. It
turns `ui.window` messages arriving over the mesh into real Wayland windows — a
markdown-over-Bus UI that any script or daemon can drive by sending a message.
It is the surface where the cosmix substrate manifests as a desktop.

## What it is

A CPU-rendering display backend built on **winit** (windowing), **softbuffer**
(pixel surface), **tiny-skia** (2D drawing), and **cosmic-text** (font shaping).
It is a *client* that paints its own windows, not a Wayland compositor that
hosts other apps.
The crate is named for its renderer; the name `cosmix-disp-wgpu` is reserved for
a future GPU backend that would speak the same protocol, but no GPU code lives
here.

It is a *consumer* of the display protocol, not the protocol itself — the shared
vocabulary of `ui.*` commands, window properties, style values, and the widget
registry lives in [`cosmix-lib-display`](libraries.md). `disp-skia` interprets
that vocabulary and paints it.

## What it does

- Connects to the local [noded](noded.md) broker and registers as the `display` service.
- Receives `ui.window` messages (and the deprecated `ui.panel` alias), each carrying a markdown body plus window properties, and renders them.
- Renders a markdown document tree through a widget set: headings, paragraphs, code blocks, blockquotes, lists, rules, tables, images — plus interactive widgets driven from the protocol (buttons, text inputs, textareas, checkboxes, toggles, radio groups, dropdowns, sliders, number inputs, tabs, accordions, treeviews, datatables, split panes, dialogs, progress bars, spinners).
- Draws window chrome: menu bar, caption buttons, scrollbars, status bar, tooltips, context menus, and resize handles.
- Tracks clickable hit regions and keyboard focus; on user interaction it sends a `ui.event` message back through the broker so the driving process can react.
- Handles `ui.style` (restyle a live window/widget), `ui.theme` (switch theme variables), and `ui.remove` (destroy a window).

## Running it

The binary installs to `/opt/cosmix/bin/cosmix-disp-skia`. It needs a running
Wayland session and a reachable Bus broker; the broker URL is resolved from
`node.toml` via `cosmix-lib-config`'s `client-helpers`, so no arguments are
required for the default case:

```sh
/opt/cosmix/bin/cosmix-disp-skia
```

Logs go to journald under the `cosmix-disp-skia` tag. On start it logs
`Registered as 'display' on broker`; if no broker is reachable the connection
attempt fails and the process exits.

Once it is up, drive it from Mix — a window is one `send`:

```mix
send display ui.window title="Hello" body="# Hello\n\nFrom the mesh."
```

## Interfaces

- **Inbound (broker → display):** `ui.window` / `ui.panel`, `ui.style`, `ui.theme`, `ui.remove`.
- **Outbound (display → broker):** `ui.event` on user interaction, plus topic subscription handling — broker-injected `topic`/`topic_seq`/`topic_stale` headers are unwrapped at the bridge boundary and used to filter late deliveries.
- **Service name:** `display` on the local node's broker.

## Where it fits

`disp-skia` is the visible edge of the substrate. Any agent, Mix script, or
daemon that can reach the broker can put a UI on screen without linking a GUI
toolkit — it just sends markdown and widget messages. This keeps the display a
message port in the ARexx tradition: addressable, scriptable, and swappable. A
different renderer (GPU, TUI, headless, WASM) can replace `disp-skia` by
consuming the same [`cosmix-lib-display`](libraries.md) protocol.

## See also

- [overview](overview.md) — the substrate at a glance
- [noded](noded.md) — the Bus broker `disp-skia` registers with
- [libraries](libraries.md) — `cosmix-lib-display` (the protocol types)
- [agentd](agentd.md) — agent supervision daemon
