---
title: ABP UI Vocabulary — `ui.*` Commands and Widget Schema
chapter: 01b
version: 0.2.0
status: retired
date: 2026-04-26
retired: 2026-08-16
amends: 0.1.0
companion: 2026-04-07-05-amp-display-protocol.md
---

# ABP UI Vocabulary — `ui.*` Commands and Widget Schema

> **RETIRED 2026-08-16 (Mark).** The `ui.*` surface this chapter specifies was
> retired from ABP by `_decisions/2026-07-18-amp-as-control-plane.md` — ABP is
> control-plane only; webd is the agent surface, and the desktop's display
> stack is the cosmix-comp compositor (chapter 16) plus the CTK toolkit (the
> 2026-07-30 Bevy+CTK decision; CTK's widget surface is out of chapter 16's
> scope). Kept verbatim as dated history (hence the `-amp-` filename stem).
> Do not cite as live protocol.

> *This chapter documents the UI vocabulary as it exists in
> `cosmix-lib-display` today. It is descriptive, not aspirational. Where
> the implementation has not crystallised an aspect of the protocol, the
> chapter flags the gap rather than inventing vocabulary.*

The wider ABP Display Protocol (state ownership, conformance levels,
end-to-end semantics) lives in `2026-04-07-05-amp-display-protocol.md`. This chapter
is the *narrow* reference: the wire shape of the `ui.*` command family,
the widget-type registry, the fenced-code-block grammar that turns a
window body into a widget tree, the action-URI scheme that dispatches link
clicks back through ABP, and the conformance tiers a backend may target.

## 0. How this chapter advances the mandate

Per the going-forward rule from CHANGELOG 2026-04-25: every spec must
state how it advances at least one of legibility / modifiability /
reconstructibility.

- **Legibility.** A new backend or audit tool that wants to reason about
  windows must know exactly which `ui.*` commands exist, which widget
  types are recognised, which properties each widget reads, and which
  link URIs dispatch where. Today that surface is implicit in
  `cosmix-lib-display` source. This chapter makes it explicit.
- **Modifiability.** Conformance tiers (§7) carve the surface into
  layers a second backend (TUI, web, alternate native renderer) can
  target incrementally. An agent proposing a new backend can read this
  chapter and know what "minimally conformant" means without reverse
  engineering `cosmix-disp-skia`.
- **Reconstructibility.** When the day comes to rebuild the display
  layer (COSMIC fork, replacement renderer, anything), this chapter is
  the contract the rebuild must keep. The substrate above the renderer
  (Mix scripts, mesh peers, agentd) talks `ui.*` and depends on the
  vocabulary documented here, not on the reference backend's specific
  implementation.

This chapter sits in **Substrate Layer 07 (self-aware)** of SPEC 07/08/09:
making the UI vocabulary queryable, addressable, and introspectable from
outside any single backend is a precondition for any future repair or
improvement pass on the display layer.

## 1. Scope and authority

**Source of truth.** `cosmix-lib-display` v0.1 (commit at 2026-04-26).
Specifically:

- `command.rs::UiCommand` — the 14 commands (§2)
- `widget.rs::WidgetType` — the 28 widget types and their fenced-code
  language hints (§3)
- `markdown.rs` and `widget.rs::parse_props` — the fenced-code-block
  widget grammar (§4)
- `window.rs::WindowProps` — window-level header schema (§5)
- `style.rs` — typed style values + theme `var(...)` resolution (§6.2)
- `action.rs::ActionUri` — the 10-form action URI scheme (§5)

**Out of scope.** State ownership, orphan handling, the `ui.subscribe` /
`ui.unsubscribe` event-filter family, conformance against the broader
display lifecycle — all in `2026-04-07-05-amp-display-protocol.md`. Topic pub/sub
itself is in `2026-04-10-03-bus-topic-pubsub.md`; this chapter only documents how
the display protocol *consumes* topics (§6.1).

**Discipline.** Where this chapter is silent on a point, the
implementation is silent. New vocabulary belongs in a future amendment,
not in a reader's inference.

---

## 2. The `ui.*` Command Family

Fourteen `ui.*` commands are defined. All are addressed to a display
service (typically `cosmix-disp-skia`) and share a common header convention:
`target` (or `id` for `ui.window`) selects the window or widget to act on;
the body carries either markdown content, JSON, or `key: value` style
pairs depending on the command.

The protocol is **idempotent on `id`**. A `ui.window` with an existing
`id` updates that window; a fresh `id` creates one. `ui.style`,
`ui.menu`, `ui.status`, `ui.context`, `ui.progress`, `ui.badge`,
`ui.scroll`, `ui.data`, and `ui.template` all replace prior state for
the same target. `ui.remove` destroys.

### 2.1 Command summary

| Command | Direction | Body shape | Purpose |
|---|---|---|---|
| `ui.window` | process → display | markdown | Create or update a window |
| `ui.style` | process → display | `key: value` lines | Restyle an existing window or widget |
| `ui.remove` | process → display | empty | Destroy a window |
| `ui.event` | display → process | freeform `key: value` | User interaction (see §8) |
| `ui.theme` | process → display | `key: value` lines | Set theme variables |
| `ui.data` | process → display | JSON array / object | Set or mutate data on a data-driven widget |
| `ui.template` | process → display | markdown | Set the per-row template for a data-driven widget |
| `ui.menu` | process → display | JSON menu array | Set the menubar for a window |
| `ui.status` | process → display | plain text, `|`-separated segments | Set status-bar text |
| `ui.context` | process → display | JSON menu array | Set the right-click context menu for a window |
| `ui.progress` | process → display | empty | Set determinate / indeterminate progress |
| `ui.badge` | process → display | empty | Set a status-bar notification badge |
| `ui.scroll` | process → display | empty | Scroll a window programmatically |
| `ui.batch` | either | JSON array of `{command, headers, body}` | Atomic multi-command (also the topic-payload convention, §6.1) |

Forward-compatibility: an unknown `ui.*` command parses as `None` from
`UiCommand::from_amp` — backends MUST silently ignore unknown `ui.*`
commands rather than erroring, so future vocabulary additions don't
break existing renderers.

### 2.2 `ui.window` — create or update a window

| Header | Required | Default | Notes |
|---|---|---|---|
| `id` | yes | — | Window identifier; idempotent — re-issuing with the same `id` updates |
| `parent` | no | (root) | Parent window `id`; `desktop` for top-level windows |
| `title` | no | — | Window title (CSD title bar) |
| `width` | no | — | px, %, or `auto` |
| `height` | no | — | px, %, or `auto` |
| `position` | no | center | `left` / `right` / `center` / `top` / `bottom` / `top-left` / `top-right` / `bottom-left` / `bottom-right` / `x,y` |
| `layout` | no | `column` | `column` / `row` / `grid` / `stack` |
| `gap` | no | — | Gap between children (rem, float) |
| `padding` | no | — | Inner padding (rem, string) |
| `align` | no | — | Child alignment |
| `scrollable` | no | `false` | `true` enables scroll container |
| `overflow` | no | — | Overflow handling |
| `decorations` | no | all enabled | Comma-separated subset of `close`, `minimize`, `maximize`, `resize`, `move`, `pin` |
| `layer` | no | `normal` | `background` / `normal` / `overlay` / `notification` |
| `sticky` | no | `false` | `true` survives workspace switches |
| `ttl` | no | — | Auto-remove after N milliseconds |
| `subscribe` | no | (none) | Topic name to bind this window to (three-state, see §6.1) |
| inline-style | no | — | `background`, `text_color`, `border_color`, `border_width`, `border_radius`, `font_size`, `opacity` headers are picked up as inline window style |

Body: markdown (parsed per §4 into a widget tree).

The `from` header of the originating message is captured as the window's
`source`; the display service uses it to route action callbacks (link
clicks, widget events) back to the creator.

**Window is not the top-level canvas.** The implicit root of the display
is `desktop` — a workspace-aware canvas owned by the display service,
not addressable as a widget. Windows are children of `desktop` by
default (when `parent` is absent or set to `desktop`) and may nest
inside other windows via `parent: <window-id>`. Workspaces are a runtime
property of the desktop root (see the `Workspace` action URI form in
§5 and the `sticky` decoration in §2.2 above) — not a widget type and
not addressable as a `parent`. A renderer that targets a non-windowed
surface (TUI, embedded view) implements the same model: a single root
canvas, with windows as the addressable composition unit underneath.

### 2.3 `ui.style` — restyle a window or widget

| Header | Required | Notes |
|---|---|---|
| `target` | yes | Window or widget `id` (glob support implementation-defined) |

Body: zero or more `key: value` lines, one per property. Values are
parsed by `style.rs::StyleValue::parse`:

- `var(name)` → kept as `Text`, resolved by the active theme at render time
- `#RGB` / `#RRGGBB` / `#RRGGBBAA` → `Color([f32; 4])`
- `scrollable` / `sticky` / `checked` / `enabled` / `visible` → `Bool`
- `gap` / `border_width` / `border_radius` / `font_size` / `opacity` /
  `min` / `max` → `Float`
- everything else → `Text`

### 2.4 `ui.remove` — destroy a window

| Header | Required | Notes |
|---|---|---|
| `target` | yes | Window `id` |

Body: empty. Removes the window and any topic binding (§6.1).

### 2.5 `ui.event` — user interaction (display → process)

Direction is **inverted**: this is the only `ui.*` command sent FROM the
display service TO the window's source process.

| Header | Required | Notes |
|---|---|---|
| `source` | yes | Window `id` the event came from |

Body: today, freeform `key: value` lines describing the interaction
(e.g. `action: select`, `row: 0`, `value: notes.md`). **The body schema
is not currently canonicalised** — see §8, "Open question: `ui.event`
payload schema."

### 2.6 `ui.theme` — set theme variables

| Header | Required | Notes |
|---|---|---|
| `name` | no | Theme name |

Body: zero or more `key: value` lines, raw strings (no type coercion;
the consumer is `Theme::from_pairs`). Used to resolve `var(name)`
references in styles.

### 2.7 `ui.data` — set or mutate data on a data-driven widget

| Header | Required | Notes |
|---|---|---|
| `target` | yes | Widget `id` (`VirtualList`, `DataTable`, `TreeView`, `TagList`) |
| `action` | no (default `replace`) | One of `replace`, `insert`, `update`, `patch`, `remove`, `clear` |
| `item` | no | Item identifier for `update` / `patch` / `remove` |

Body: JSON array (for `replace` / `insert`) or JSON object (for
`update` / `patch` of a single item). For `clear`, body is empty.

### 2.8 `ui.template` — set per-row template for a data-driven widget

| Header | Required | Notes |
|---|---|---|
| `target` | yes | Widget `id` |

Body: markdown template. `{field}` placeholders are interpolated against
each row from the corresponding `ui.data` payload. (Template language
itself is implementation-defined; this chapter does not pin the
interpolation rules beyond `{field}` substitution.)

### 2.9 `ui.menu` — set the menubar for a window

| Header | Required | Notes |
|---|---|---|
| `target` | yes | Window `id` |

Body: JSON menu array. Schema is implementation-defined
(reference-backend-specific shape today). **Gap** — a canonical menu
schema is not in `cosmix-lib-display`; it lives in `cosmix-disp-skia`.
A future amendment should hoist the schema into the spec.

### 2.10 `ui.status` — set status-bar text for a window

| Header | Required | Notes |
|---|---|---|
| `target` | yes | Window `id` |

Body: plain text. The `|` character separates segments
(left-aligned... right-aligned).

### 2.11 `ui.context` — set right-click context menu for a window

Same shape as `ui.menu`. Same gap on schema location.

### 2.12 `ui.progress` — set the progress indicator on a window's status bar

| Header | Required | Default | Notes |
|---|---|---|---|
| `target` | yes | — | Window `id` |
| `value` | no | `0.0` | Float in `0.0..=1.0` for determinate progress |
| `indeterminate` | no | `false` | `true` shows the indeterminate animation; `value` is ignored |

Body: empty.

### 2.13 `ui.badge` — set a notification badge on a window's status bar

| Header | Required | Default | Notes |
|---|---|---|---|
| `target` | yes | — | Window `id` |
| `count` | no | `0` | Integer; `0` hides the badge |
| `color` | no | (red) | ARGB hex (e.g. `#ff4444`) |

Body: empty.

### 2.14 `ui.scroll` — scroll a window programmatically

| Header | Required | Default | Notes |
|---|---|---|---|
| `target` | yes | — | Window `id` |
| `pos` | no | `top` | `top` / `bottom` / `<float pixels>` |

Body: empty.

### 2.15 `ui.batch` — atomic multi-command

Body: JSON array of `{command, headers, body}` objects. Each element is
re-assembled into an `AmpMessage` and re-parsed through
`UiCommand::from_amp`. This means:

- Forward-compatibility for free: a future `ui.*` command in a batch
  parses through the same dispatch.
- Recursive: a `ui.batch` may contain a `ui.batch`. (Implementation
  caveat: nesting depth is bounded by the wire format's stream framing;
  see `2026-03-24-01-bus-wire-protocol.md` §5.2 and the topic-payload nesting
  constraint in `2026-04-10-03-bus-topic-pubsub.md` §3.11.2.)
- Unknown commands in a batch are silently dropped, not errored. This
  matches the forward-compatibility rule from §2.1.

`ui.batch` is also the canonical **topic-payload shape** (see §6.1).

---

## 3. The Widget Type Registry

`widget.rs::WidgetType` enumerates the recognised widget types — 28 as
of v0.1.0, organised in seven categories. The registry is **expected
to grow**: this chapter pins the v0.1.0 set, but new widget types are
added by minor-version amendment (see §9) without breaking existing
backends, since `WidgetType::from_language_hint` returns `None` for
unrecognised types and `markdown::parse` falls back to a passive
`CodeBlock` node — the silent-ignore rule of §2.1 applied at the
widget level.

A fenced code block in a window body whose language hint matches a
recognised widget name (or one of its accepted aliases) is parsed as
an `InteractiveWidget` node by `markdown.rs::parse`.

### 3.1 Aliases

The language hint may be the canonical name or any documented alias. The
full alias map (from `WidgetType::from_language_hint`):

| Canonical | Aliases |
|---|---|
| `Image` | `image`, `img` |
| `Markdown` | `markdown`, `md` |
| `Container` | `container`, `div` |
| `ScrollContainer` | `scroll`, `scrollcontainer` |
| `SplitPane` | `splitpane`, `split` |
| `Button` | `button`, `btn` |
| `TextInput` | `textinput`, `input` |
| `Dropdown` | `dropdown`, `select` |
| `Toggle` | `toggle`, `switch` |
| `Slider` | `slider`, `range` |
| `NumberInput` | `numberinput`, `number`, `stepper` |
| `RadioGroup` | `radiogroup`, `radio` |
| `VirtualList` | `virtuallist`, `vlist` |
| `DataTable` | `datatable`, `table` |
| `TreeView` | `treeview`, `tree` |
| `TagList` | `taglist`, `tags`, `chips` |
| `ProgressBar` | `progressbar`, `progress` |
| `Spinner` | `spinner`, `loading` |
| `Breadcrumb` | `breadcrumb`, `crumbs` |
| `Accordion` | `accordion`, `collapsible` |
| `Dialog` | `dialog`, `modal` |

All other types use their lowercase name (`label`, `icon`, `tabs`,
`textarea`, `checkbox`, `menubar`, `contextmenu`).

### 3.2 Per-widget property schema

Property schemas below describe what the **archived reference backend**
consumed. The `ui.*` rendering lane was retired by
`_decisions/2026-07-18-amp-as-control-plane.md`; historical source remains at
cos tag `amp-display-archive`. Current CosMix Desktop applications render
natively through Bevy/wgpu and CTK. In this historical vocabulary, `id` is
universal (every interactive widget needs it for event routing and
`ui.style` / `ui.data` targeting); other props listed were read
by the reference backend. A second backend conforming to this
spec MUST honour `id` and SHOULD honour every prop listed; props
it cannot render meaningfully it MUST silently ignore.

#### Display widgets

| Widget | Required | Optional | Body |
|---|---|---|---|
| **Label** | `id` | `style` (`bold`/`italic`/`mono`, default normal), `color` (#hex), `text` | text content (used if `text` prop absent) |
| **Icon** | `id` | `name`, `size` (`small`/`medium`/`large`), `color` (#hex) | icon name (used if `name` prop absent) |
| **Image** | `id` | `src`, `alt` | image source URL (used if `src` prop absent) |
| **Markdown** | `id` | (none) | markdown content (parsed recursively per §4) |

#### Layout widgets

| Widget | Required | Optional | Body |
|---|---|---|---|
| **Container** | `id` | `padding` (px), `background` (#hex), `border` (#hex), `radius` (px) | markdown rendered as children |
| **ScrollContainer** | `id` | (none) | **Gap** — declared in `WidgetType` but no `cosmix-disp-skia` implementation today; renders as placeholder |
| **Tabs** | `id` | `active` (tab index, default 0) | tab labels as `- ` lines, then `---`, then sections separated by `---` |
| **SplitPane** | `id` | `direction` (`horizontal`/`vertical`), `split` (ratio 0.05–0.95) | left/top content, `---`, right/bottom content |

#### Input widgets

| Widget | Required | Optional | Body |
|---|---|---|---|
| **Button** | `id` | `label`, `variant` (`default`/`primary`/`danger`/`ghost`), `disabled` | (ignored) |
| **TextInput** | `id` | `placeholder`, `value`, `disabled` | initial value (used if `value` prop absent) |
| **TextArea** | `id` | `placeholder`, `value`, `rows` (default 5), `disabled` | initial value (used if `value` prop absent) |
| **Dropdown** | `id` | `options` (CSV list), `value`, `placeholder`, `label`, `disabled` | (ignored) |
| **Checkbox** | `id` | `label`, `checked`, `disabled` | (ignored) |
| **Toggle** | `id` | `label`, `checked`, `disabled` | (ignored) |
| **Slider** | `id` | `min`, `max`, `step`, `value`, `label`, `disabled` | (ignored) |
| **NumberInput** | `id` | `value`, `min`, `max`, `step`, `label`, `disabled` | (ignored) |
| **RadioGroup** | `id` | `options` (CSV), `value`, `direction` (`vertical`/`horizontal`), `disabled` | (ignored) |

#### Data widgets

Data widgets declare a shell via fenced code block; their content is
populated through `ui.data` / `ui.template` (§2.7–2.8).

| Widget | Required | Optional | Body |
|---|---|---|---|
| **VirtualList** | `id` | (none consumed inline) | (ignored; populated via `ui.data` + `ui.template`) |
| **DataTable** | `id` | `sortable`, `selectable` (`none`/`single`/`multi`), `columns` (JSON or CSV), `page_size`, `total_rows` | `key: value` lines merged into props |
| **TreeView** | `id` | `selectable` | (ignored; populated via `ui.data` JSON tree) |
| **TagList** | `id` | `removable` (default `true`), `color` (#hex) | (ignored; populated via `ui.data`) |

#### Feedback widgets

| Widget | Required | Optional | Body |
|---|---|---|---|
| **ProgressBar** | `id` | `value` (0..max), `max` (default 100), `label`, `indeterminate` | (ignored) |
| **Spinner** | `id` | `size` (`small`/`medium`/`large`), `label` | (ignored) |

#### Navigation widgets

| Widget | Required | Optional | Body |
|---|---|---|---|
| **Breadcrumb** | `id` | `separator` (default `" > "`) | one segment per line; `[text](url)` lines are clickable, plain text is current |
| **Accordion** | `id` | `expanded` (CSV indices), `multiple` (default `true`) | section labels as `- ` lines, then `---`, then sections separated by `---` |

#### Chrome widgets

| Widget | Required | Optional | Body |
|---|---|---|---|
| **Dialog** | `id` | `title`, `width` (default 400), `closable` (default `true`) | markdown rendered as dialog body |
| **MenuBar** | — | — | populated via `ui.menu`, not as a fenced widget |
| **ContextMenu** | — | — | populated via `ui.context`, not as a fenced widget |

### 3.3 Adding new widget types

Adding a widget type is an **additive amendment** to this chapter — a
0.x.0 minor bump (see §9), not a major version. The procedure:

1. Land the implementation in `cosmix-lib-display::WidgetType` (enum
   variant, alias mapping in `from_language_hint`) and at least the
   reference `cosmix-disp-skia` backend.
2. Add a row to the appropriate category table in §3.2 with the same
   shape as existing rows (canonical name, required props, optional
   props, body semantics).
3. If the new widget belongs in `core` (§7.1), add it there as well —
   otherwise it lands implicitly in `core+data+chrome` (§7.3) which
   covers the full registry.
4. Bump this chapter's `version` field and add a CHANGELOG entry.

A backend that does not implement the new type continues to conform at
its prior tier; the unknown widget falls through to a passive
`CodeBlock` node and renders as inert content. This is what makes the
registry safe to grow.

### 3.4 Gaps in the registry

Three of the 28 declared types do not have a complete fenced-widget
implementation in `cosmix-disp-skia` today:

- **ScrollContainer** is declared but unimplemented; renders as a
  placeholder. Either it acquires an implementation or it should be
  retired from the registry — flagging for resolution.
- **MenuBar** and **ContextMenu** are populated through their dedicated
  commands (`ui.menu`, `ui.context`), not declared as widgets in window
  bodies. The registry entries exist for future symmetry; today they
  are command-driven.

---

## 4. Fenced-code-block widget grammar

Window bodies (§2.2) are GFM markdown. Most markdown constructs map to
passive widget nodes (heading, paragraph, list item, blockquote, table,
horizontal rule, image, inline code) — the renderer decides how to
draw them. The single active extension is the **fenced code block as
widget declaration**.

### 4.1 Grammar

A fenced code block

```` text
```<lang> [<key>=<value> [<key>="quoted value"]...]
<content>
```
````

is parsed as follows:

- The first whitespace-separated token of the language hint is the
  **widget type**. It matches against `WidgetType::from_language_hint`
  (§3.1). If unrecognised, the block falls back to a passive
  `CodeBlock` node with the language preserved.
- The remainder of the language hint is parsed by
  `WidgetType::parse_props` into ordered `(key, value)` pairs. Values
  may be unquoted (terminated by whitespace) or double-quoted (allowing
  whitespace inside).
- The fenced content is captured verbatim as the widget's `content`
  string. Most interactive widgets ignore content (state lives in
  props or in `ui.data`); a small set (Container, Markdown, Tabs,
  SplitPane, Accordion, Breadcrumb, Dialog) parse content to populate
  their children.

### 4.2 Worked example

```` text
```textinput id=to placeholder="To..."
```
````

parses to:

- widget type: `TextInput`
- props: `[("id", "to"), ("placeholder", "To...")]`
- content: `""`

A `Container` with children:

```` text
```container id=hbox padding=8 background=#1a1a2e
# Header

[Click me](some.action)
```
````

renders the fenced content through `markdown::parse` recursively,
producing a heading node and an action paragraph inside the container.

### 4.3 Inline spans

Within paragraphs, the parser emits `Span::Action` for `[text](url)`
links — every link in a window body is an actionable button whose URL
follows §5. `[text](url "title")` carries the title as `tooltip`.
`![alt](src)` produces `Span::Image`. Other inline formatting
(`Bold`, `Italic`, `Strikethrough`, `Code`, `Break`) is passive
styling.

### 4.4 Idempotency note

The fenced-code grammar makes the markdown body the **declarative
shape of the widget tree**. Re-issuing `ui.window` with the same `id`
and a fresh body replaces the tree wholesale; widget state for `id`s
that exist in both bodies is preserved by the renderer (this is a
`cosmix-disp-skia` implementation property, not a wire guarantee — see
the open question in §8 about state-preservation semantics).

---

## 5. Action URI scheme

Every link URL inside a window body is an action. `action.rs::ActionUri`
parses ten distinct forms; the renderer dispatches by form.

| # | Form | Example | Dispatch |
|---|---|---|---|
| 1 | `AmpCommand` | `maild.send`, `files.delete:notes.md` | Direct ABP command (any service-prefixed command per SPEC 02), optional `:param` after a dotted command name |
| 2 | `WindowNav` | `ui.window:settings` | Open or focus the named window locally |
| 3 | `Launch` | `launch:cosmix-mail`, `launch:ssh:server-03` | Spawn a process (target + colon-separated args) |
| 4 | `XdgOpen` | `xdg-open:https://example.com` | Pass to system handler (browser, file manager) |
| 5 | `AmpReply` | `amp-reply:status.refresh` | **Federated**: compose ABP-encoded reply email with the named command — see §5.1 |
| 6 | `AmpRequest` | `amp-request:logs.tail` | **Federated**: WebSocket request if a live mesh connection exists — see §5.1 |
| 7 | `Mailto` | `mailto:mark@cosmix.mesh` | Open mail composer with plain-text recipient |
| 8 | `ExternalUrl` | `https://example.com` | Treat as `XdgOpen` (any `http://` or `https://`) |
| 9 | `Workspace` | `workspace:1` | Switch to the named workspace |
| 10 | `Remote` | `remote:server-03:systemctl:restart:nginx` | **Federated**: invoke a command on a named mesh peer — see §5.1 |

Three accommodations from `ActionUri::parse`:

- An optional `amp:` scheme prefix is stripped before dispatch
  (`amp:maild.send` ≡ `maild.send`).
- `maild.reply:msg-123` parses as `AmpCommand { command: "maild.reply",
  param: Some("msg-123") }` because the first token contains a `.` (it
  looks like a dotted command name). A token without `.` followed by
  `:param` parses as the whole string as a plain `AmpCommand` with no
  param.
- `ActionUri::amp_command()` exposes the ABP command for the three
  ABP-dispatchable forms (`AmpCommand`, `AmpReply`, `AmpRequest`),
  letting renderers route those uniformly.

### 5.1 The federation tier

`AmpReply`, `AmpRequest`, and `Remote` are the load-bearing forms for
agent and federation use cases. Their semantics need careful
treatment because they are the bridge between local-process
interactions and cross-mesh / asynchronous-channel interactions.

**`AmpReply`** is the asynchronous federation form. The renderer
composes an ABP-encoded reply email (or other deliverable channel) to
the window's source process and queues it for delivery; if the source
is offline or in a different mesh, the message survives the gap. Use
case: a status window rendered from a maild-delivered ABP message,
where clicking a button generates a reply that travels back over
maild even if the originating peer is currently disconnected.

**`AmpRequest`** is the synchronous federation form. The renderer
issues a live WebSocket request to the window's source if a connection
exists; if not, dispatch fails with no fallback (the renderer reports
the failure as a `ui.event` with a structured error — see the §8 open
question for what that structured shape should be). Use case: a
real-time control window where the user expects immediate
acknowledgement and a missing producer is a hard error worth
surfacing.

**`Remote`** is the cross-peer dispatch form. `remote:<peer>:<command>`
sends `<command>` to the named mesh peer over the existing ABP routing
fabric — i.e. this is shorthand for an ABP message with `to:
<command>@<peer>`. The renderer does not care whether `<peer>` is
local or remote; the broker resolves it. Failure (peer unreachable, no
service registered) propagates back as the same error shape any ABP
request would surface.

These three forms together are why action URIs are not just "links to
commands" but a federation primitive. A window rendered from a remote
peer's `ui.window` message can carry buttons whose URIs route back
through three distinct delivery channels (sync mesh request, async
mesh reply, peer-targeted request) without the renderer needing to
know which is appropriate. The action URI itself encodes the choice.

---

## 6. Topic interaction and `ui.batch`

This chapter does not specify the topic broker — that is
`2026-04-10-03-bus-topic-pubsub.md`. But the broker is observable in two places
inside the UI vocabulary, and those places need pinning down here.

### 6.1 `WindowProps.subscribe` — three-state semantics

The `subscribe` header on `ui.window` (§2.2) binds the window to a broker
topic. Per `WindowProps::from_headers`, the parsed value is
`Option<String>` with three meaningful states:

| Header form | Parsed value | Semantics on update |
|---|---|---|
| absent | `None` | preserve any existing binding (no-op) |
| `subscribe: name` | `Some("name")` | bind to `name`; if a different binding exists, atomic swap |
| `subscribe: ` (empty value) | `Some("")` | explicitly clear any existing binding |

This three-state encoding is structurally identical to the
update-vs-clear distinction used elsewhere in ABP and is the source of
the "absent ≠ explicitly empty" rule that subscribers' lifecycle code
depends on. See `2026-04-10-03-bus-topic-pubsub.md` §3.1.2 for the full lifecycle
semantics — this chapter only documents the parsing.

### 6.2 `ui.batch` as topic-payload convention

Per `2026-04-10-03-bus-topic-pubsub.md` §3.11.2, the canonical body shape for a
`topic.publish` message is a complete inner ABP message — most
commonly `ui.batch`. The broker injects routing headers
(`topic: <name>`, `topic_seq: <n>`) into the inner message and fans
the annotated form out to subscribers. Subscribers see a normal
`ui.batch` and dispatch through their existing `ui.batch` handler;
the inner sub-commands (`ui.update`, `ui.data`, `ui.style`, etc.) run
as if they had arrived directly.

This is the **topic / UI loop closure**: a producer publishes a single
`ui.batch` to a topic; every subscribed window receives the batched
update; the renderer applies it atomically. No bespoke "topic
delivery" command on the renderer side. The `topic_*` headers are
informative — a renderer MAY use them to gate or log deliveries but
MUST NOT require them for dispatch.

---

## 7. Backend conformance tiers

A backend is any consumer of the `ui.*` vocabulary that draws windows
or otherwise materialises the protocol. `cosmix-disp-skia` is the
reference backend; this section names the conformance tiers a second backend
(TUI, web renderer, alternate native renderer) MAY target.

The three tiers are cumulative — `core+data` includes everything in
`core`; `core+data+chrome` includes everything in `core+data`.

### 7.1 Tier 1 — `core`

A `core` backend implements the window/widget surface needed for any
useful interactive UI. This is the minimum viable conformance level;
a TUI backend or screenreader bridge should target this tier first.

**Required commands:** `ui.window`, `ui.style`, `ui.remove`, `ui.event`,
`ui.theme`, `ui.batch`.

**Required widget types:** ten widgets covering display, layout, and
input fundamentals.

| Category | Widgets |
|---|---|
| Display | Label, Icon, Markdown |
| Layout | Container, Tabs, SplitPane |
| Input | Button, TextInput, Checkbox, Dropdown |

**Required behaviours:**

- Idempotency on `id` for `ui.window`.
- Action URI dispatch for forms 1, 2, 4, 7, 8 (`AmpCommand`,
  `WindowNav`, `XdgOpen`, `Mailto`, `ExternalUrl`).
- `ui.theme` `var(name)` resolution per §6.2 of source-of-truth (`style.rs`).
- Forward-compatible silent ignore of unknown `ui.*` commands and
  unknown widget types.

**Optional at this tier:** the federation action forms
(`AmpReply` / `AmpRequest` / `Remote`), `Launch`, `Workspace`. A
backend MAY surface them as no-ops or as a generic error event back
through `ui.event`.

### 7.2 Tier 2 — `core+data`

Extends `core` with data-driven widgets and their support commands.
Suitable for a backend that wants to render dashboards, lists,
trees, and tables.

**Adds commands:** `ui.data`, `ui.template`.

**Adds widget types:** VirtualList, DataTable, TreeView, TagList.

**Adds behaviours:**

- All six `DataAction` variants on `ui.data`: `replace`, `insert`,
  `update`, `patch`, `remove`, `clear`.
- `{field}` placeholder substitution in `ui.template` bodies.
- `WindowProps.subscribe` and the `ui.batch`-as-topic-payload
  convention (§6) — without this, dashboards can't auto-update.

### 7.3 Tier 3 — `core+data+chrome`

Adds the desktop-chrome surface — menus, status bars, progress,
badges, and modal dialogs. This is the tier `cosmix-disp-skia` targets today.

**Adds commands:** `ui.menu`, `ui.status`, `ui.context`,
`ui.progress`, `ui.badge`, `ui.scroll`.

**Adds widget types:** the full registry: NumberInput, RadioGroup,
Slider, TextArea, Toggle, Dialog, ProgressBar, Spinner, Breadcrumb,
Accordion, Image (already in display), MenuBar, ContextMenu (the
last two are command-driven, not fenced).

**Adds behaviours:**

- All ten action URI forms.
- All decorations / layers / position values from §2.2.
- `ui.scroll` with both `top` / `bottom` and pixel destinations.

### 7.4 Tier orthogonal — none

There is deliberately no "tier 0" or "headless" tier here. A backend
that draws nothing is not a UI backend — it's a different kind of ABP
consumer (an audit log, an indexer, a test harness). This chapter is
silent about such consumers; they read ABP messages, not `ui.*`
commands per se.

### 7.5 Backend naming convention

Display-backend crates are named `cosmix-disp-{framework}` where
`{framework}` is the **primary framework or rendering substrate** the
backend is built on. Naming by framework rather than by interface
mode (gui / tui / web) is deliberate: a single "GUI" tier collapses
real distinctions across wgpu, egui, slint, dioxus, qt6, gtk4, and a
dozen others that share nothing at the implementation level.
Framework-naming scales — every backend names itself after what it
is built on, this chapter remains the conformance contract, and a
backend-agnostic harness adjudicates parity.

**Suffix rules:**

- The framework's own lowercase name (e.g. `egui`, `slint`,
  `iced`, `dioxus`, `xilem`, `ratatui`).
- Version number included only when intrinsic to the framework's
  identity (`qt6`, `gtk4`).
- For a renderer with no higher-level framework, the suffix is the
  distinguishing rasteriser. The current reference backend is a CPU
  stack (`winit` + `softbuffer` + `tiny-skia` + `cosmic-text`) and is
  named `cosmix-disp-skia` for its `tiny-skia` rasteriser. The `wgpu`
  suffix is reserved for a future GPU backend (`winit` + `wgpu` +
  `taffy`) — `winit` is implied either way, `taffy` is shared layout
  machinery, and the rasteriser (`skia` vs `wgpu`) is what makes each
  backend distinct from the others and from the framework-mediated
  alternatives.
- Where one framework supports multiple targets (e.g. dioxus
  desktop vs dioxus web), the suffix is framework-only when
  unambiguous and framework + target only when both variants
  ship simultaneously.

**Examples — current and forward:**

| Crate                   | Framework / substrate                     |
|-------------------------|-------------------------------------------|
| `cosmix-disp-skia`      | Archived CPU reference renderer; source retained at cos tag `amp-display-archive`. |
| `cosmix-disp-ratatui`   | Not built; retired with the `ui.*` lane by `_decisions/2026-07-18-amp-as-control-plane.md`. |
| `cosmix-disp-egui`      | egui (immediate-mode, future)             |
| `cosmix-disp-slint`     | slint (declarative DSL, future)           |
| `cosmix-disp-iced`      | iced (Elm-architecture, future)           |
| `cosmix-disp-dioxus`    | dioxus (React-like, future)               |
| `cosmix-disp-xilem`     | xilem (future)                            |
| `cosmix-disp-qt6`       | Qt6 widgets (future)                      |
| `cosmix-disp-qml`       | Qt Quick / QML (future)                   |
| `cosmix-disp-gtk4`      | GTK4 (future)                             |
| `cosmix-disp-cocoa`     | NSWindow / NSView, macOS (future)         |
| `cosmix-disp-html`      | browser DOM (future, ≠ `cosmix-webd`)     |
| `cosmix-disp-react`     | React, web (future)                       |
| `cosmix-disp-notcurses` | notcurses-rs, alt TUI (future)            |

The shared protocol library is and remains `cosmix-lib-display` — one
protocol library, many backends. The reference implementation since
v0.2.0 is `cosmix-disp-skia` (the v0.2.0 amendment renamed
`cosmix-deskd` → `cosmix-disp-wgpu` alongside the windowing-vocabulary
rename — see §9 and `_decisions/2026-04-27-windowing-vocabulary.md`; the
`wgpu` token was corrected to `skia` on 2026-05-19 to match the actual
CPU rasteriser — see `_spec/CHANGELOG.md`).

---

## 8. Open question — `ui.event` payload schema

**This section flags the single design question this chapter does not
resolve.**

Today, `ui.event` body is a free-form `key: value` stream
(`UiCommand::Event { source, body }` in `command.rs`). In the round-trip
test, the body is:

```text
action: select
row: 0
value: notes.md
```

This works because sender (`cosmix-disp-skia`) and receiver (the window's source
process) agree by convention. As soon as a second backend exists, that
convention is no longer reliable: a TUI backend may emit
`action: keypress`, a web backend may emit `action: click` with
different fields, and windows won't survive a backend switch.

**Recommendation (not ratification):** structure events the same way
inbound commands are structured — as **ABP headers**, not as body
content. The shape would be:

```text
---
command: ui.event
source: <window-id>
action: <verb>             # click, select, change, submit, keypress, ...
target: <widget-id>        # the widget the event came from
value: <stringified value> # canonical text representation
---
<optional large payload>
```

Rationale:

- ABP headers are the documented way to carry structured fields
  (§5.5.1 of `2026-03-24-01-bus-wire-protocol.md`); the body is for prose,
  markdown, or large opaque payloads. Today's `ui.event` inverts that
  rule — it puts structured fields in the body.
- A canonical `(action, target, value)` triple covers the high-99% of
  events and is renderable in a single grep across window sources.
- Larger or richer event payloads (a multi-row selection, a drag
  delta, a structured form submission) can ride in the body as JSON
  while the headers continue to identify the event for routing /
  filtering.
- `ui.subscribe` (in `2026-04-07-05-amp-display-protocol.md`) filters event
  streams by `(source_window, action)`. The filter predicate already
  assumes `action` is a discrete identifier; promoting it to a header
  makes the filter implementable without parsing every event body.

**Status:** flagged as **needs ratification**. This chapter does not
declare the new schema canonical. A subsequent amendment should:

1. Confirm the canonical header set (`action`, `target`, `value`, and
   any others — `modifiers` for keyboard events? `coords` for pointer
   events? `error` for `AmpRequest` failure paths from §5.1?).
2. Specify the body convention for events with structured payloads.
3. Update `cosmix-lib-display::UiCommand::Event` to parse these
   headers explicitly rather than copying the entire body.
4. Migrate existing event consumers (`mix on` handlers, agentd event
   listeners) to read the new headers.

Until that amendment lands, backends emitting `ui.event` SHOULD
include `action`, `target`, and `value` as both headers AND body lines
— belt and braces — so consumers written either way keep working.

---

## 9. Versioning

This chapter is `version: 0.2.0`, `status: draft`, amending `0.1.0`.
The vocabulary it documents is the production rename: today's
production renderer is `cosmix-disp-skia` registered as the
`display` service, running on a CPU stack (winit + softbuffer +
tiny-skia + cosmic-text) — hence the `cosmix-disp-skia` name. A
genuinely GPU-backed renderer (wgpu/taffy) would be a separate
`cosmix-disp-wgpu` crate, tracked in the deskd-rewrite plan (removed with the
old `src/_doc/planned/` tree —
`git -C $CMCTL show f18e7443^:src/_doc/planned/deskd-rewrite.md`); it is
not the production binary. The alias rules below
(`ui.panel` → `ui.window`, `cosmix-deskd` → `cosmix-disp-wgpu` →
`cosmix-disp-skia`)
let producers and consumers cut over independently. Bumps:

- **0.1.x** — clarifications, gaps closed, no semantic change.
- **0.2.0** *(this revision, 2026-04-26)* — two of three queued v0.2.x
  amendments landed together: (b) the windowing-vocabulary rename —
  `Panel` retires as the generic top-level term, the canonical command
  is `ui.window` (was `ui.panel`), and the surface-property struct is
  `WindowProps` (was `PanelProps`); and (c) the reference-backend
  rename `cosmix-deskd` → `cosmix-disp-wgpu` per the §7.5
  framework-naming convention (later retoken'd `cosmix-disp-wgpu` →
  `cosmix-disp-skia`, 2026-05-19 — see `CHANGELOG.md`). Both renames
  are mechanical (no
  behavioural change). Backends MUST accept `ui.panel` as a
  deprecated alias for `ui.window` and parse it to the same
  `UiCommand` variant; producers SHOULD emit `ui.window`. The alias
  is retained through the 0.2.x line and removed at 1.0.0.
- **0.3.0 (next queue)** — (a) `ui.event` payload-schema ratification
  per §8; and the substantive *split* foreshadowed by the Wayland
  five-noun vocabulary (`Output / Window / Pane / Layer / Popup`) per
  `_decisions/2026-04-27-windowing-vocabulary.md`: carve `LayerProps` out of
  `WindowProps` and apply Wayland-precedence rules for window
  properties (minimise, pin, decoration ownership). Unlike v0.2.0,
  this is a behavioural change — distinct surface types with distinct
  property schemas — and warrants its own minor bump.
- **0.x.0 (minor)** — additive growth: new widget types in the
  registry (§3.3 procedure), new `ui.*` commands, new action URI
  forms, new conformance tiers as second backends materialise.
  Additive amendments do **not** invalidate existing backends — the
  silent-ignore rules in §2.1 (unknown commands) and §3 (unknown
  widget types) make growth safe.
- **1.0.0** — promote to `status: stable` once `ui.event` is
  ratified, the `WindowProps`/`LayerProps` split has landed, a
  second backend has shipped against `core`, and the ScrollContainer
  / MenuBar / ContextMenu gaps in §3.4 are resolved. Deprecated
  aliases (notably `ui.panel`) are removed at 1.0.0.

Constitutional changes (anything that affects mesh routing, trust
domain, or the wire format itself) belong in `2026-03-24-01-bus-wire-protocol.md`,
not here.

---

## 10. References

- `2026-03-24-01-bus-wire-protocol.md` — wire format, header rules, JSON-in-headers
- `2026-03-29-02-bus-command-vocabulary.md` — naming convention for all ABP commands
- `2026-04-10-03-bus-topic-pubsub.md` — topic broker, `subscribe` lifecycle, `ui.batch`
  as topic payload
- `2026-04-07-05-amp-display-protocol.md` — full display protocol: state ownership,
  orphan handling, `ui.subscribe` event-filter family, end-to-end
  conformance
- `cosmix-lib-display` source — the authoritative reference for the
  vocabulary documented here
- the retired mesh-headless-classification decision (2026-07-23, git history) — where the display
  layer sits in the crate dependency graph

---

*This chapter was drafted as a documentation pass over existing
`cosmix-lib-display` code, not as a forward design. Where the chapter
disagrees with `cosmix-lib-display` source, the source wins and the
chapter is wrong; report and amend.*
