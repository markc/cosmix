---
title: Windowing Vocabulary — Wayland-aligned, AmigaOS-rooted
date: 2026-04-26
status: directional
next_review: 2026-07-26
draws_from: ["_spec/2026-04-27-01b-amp-ui-vocabulary.md", "_spec/2026-04-07-05-amp-display-protocol.md", "CLAUDE.md"]
tags: ["display", "vocabulary", "wayland", "amigaos", "intuition", "panel", "window", "layer-shell", "decision-record"]
---

# Windowing Vocabulary — Wayland-aligned, AmigaOS-rooted

> **MOOT (2026-07-18):**
> [`2026-07-18-amp-as-control-plane.md`](./2026-07-18-amp-as-control-plane.md)
> retires the `ui.*` rendering path, so there is no substrate windowing
> vocabulary left to name. Kept as lineage only.

> **Status: directional design memo (relocated from `_doc-old/`).**
> Original date 2026-04-26. Companion to the v0.2.x ABP UI
> vocabulary spec (`../_spec/2026-04-27-01b-amp-ui-vocabulary.md`); not
> canon. Surfaces the Wayland-vs-AmigaOS naming question that the
> 01b draft deferred. Lineage only; current direction is
> `2026-07-18-amp-as-control-plane.md`.
>
> **Already landed (do not read this memo as pending work):**
> the `ui.panel` → `ui.window` rename has shipped (the deprecated
> `ui.panel` alias is accepted through the 0.2.x line), and the
> `cosmix-deskd` binary has been renamed to `cosmix-disp-skia`
> (via an interim `cosmix-disp-wgpu` name; registered as the
> `display` service). The five-noun
> `Output / Window / Pane / Layer / Popup` proposal in this memo is
> still directional and informs a future 01b amendment, but the
> two terminology shifts above are no longer proposals — they are
> the working state. Future-tense phrasing in the body should be
> read with that disclaimer in mind.

Input memo for a future v0.2.x amendment of `_spec/2026-04-27-01b-amp-ui-vocabulary.md`.
Surfaces a vocabulary collision between Cosmix's current "Panel-as-toplevel"
phrasing and the modern Wayland shell ecosystem, and proposes a five-noun
vocabulary (`Output / Window / Pane / Layer / Popup`) that aligns with
Wayland while preserving the AmigaOS lineage that motivates the project.
Not canon — directional. Companion to the 01b draft, not a replacement.

## Why this memo exists

Drafting `_spec/2026-04-27-01b-amp-ui-vocabulary.md` surfaced the question of whether
`Panel` is the right top-level container. The spec calls `Panel` "the
top-level addressable container" because that is what `cosmix-lib-display`
currently models, but the operator noted — correctly — that a desktop or
workspace, not a panel, ought to be the root canvas. The 01b draft was
amended to clarify that `desktop` is the implicit root and `Panel` is one
addressable surface within it; this memo captures the deeper question that
clarification deferred.

Two threads of research were commissioned to inform the answer:

1. **AmigaOS Intuition** — the project's stated ergonomic precedent. What
   does AmigaOS actually call its windowing primitives, and what does the
   Cosmix proclamation inherit by claiming the lineage?
2. **Wayland and modern shells** — the substrate Cosmix runs on today,
   and the vocabulary every neighbouring project (GNOME, KDE, Hyprland,
   Sway, niri, COSMIC) already uses. Per the operator's explicit
   instruction, **Wayland takes precedence over AmigaOS where the two
   diverge**.

The findings, taken together, point at the same answer from both sides.

## AmigaOS lineage (background)

Intuition's hierarchy is two-tiered:

```
Screen → Window → (Gadget | Menu | Requester)
```

- **Screen** — a full-resolution display surface with its own colour map,
  resolution, and pull-down menu bar. Multiple Screens are stacked Z-axis
  and dragged down to switch contexts. The closest thing to a "workspace"
  the OS has.
- **Window** — a draggable, resizable rectangle on a Screen. Owns its own
  rendering, input focus, and (optionally) ARexx port.
- **Gadget** — every interactive element inside a Window: buttons, knobs,
  scrollbars, the close box. Comparable to a widget.
- **Requester** — a transient modal overlay (think dialog or popup).
- **Menu** — pull-down items hung off the Screen-level title bar, *not*
  the Window — a key Intuition divergence from the Mac/Windows model.

Notable absences: no `Panel` tier, no minimise (windows can be sent to
back via depth gadget but not iconified to a dock), no always-on-top
property (only Screen-level Z-order). ARexx ports are owned per-application
and addressable by name — the property Cosmix's ABP model directly
descends from.

The lineage relevant to Cosmix is the *port-per-application* property,
not the specific noun list. Intuition's `Screen / Window / Gadget` carries
neither the layer-shell distinction modern desktops require nor the
nested-pane vocabulary every IDE-style application now needs.

## Wayland and modern shells (precedence)

Wayland's protocol-level hierarchy:

```
wl_display
  └─ wl_output                   (one per physical monitor)
       └─ wl_surface             (generic drawable; needs a role)
            ├─ xdg_toplevel      (regular app window)
            ├─ zwlr_layer_surface_v1   (layer-shell: bars, docks, wallpaper, lockscreen, notifications)
            ├─ xdg_popup         (transient: menus, tooltips, autocomplete)
            └─ wl_subsurface     (nested surface inside another)
```

A *workspace* is **not in the protocol** — it is compositor-private state
(GNOME `org.gnome.Shell.Extensions.Workspaces`, KDE `KWin::VirtualDesktops`,
niri's columnar workspaces, COSMIC's tiles). Anything Cosmix calls
"workspace" lives at the same architectural tier as a window manager,
not at a Wayland-protocol tier.

What modern shells call things, with notable convergence:

| Term       | Used by                              | Means                                    |
|------------|--------------------------------------|------------------------------------------|
| **Output** | wlroots, Sway, Hyprland, niri        | Physical monitor (`wl_output`)           |
| **Window** | every shell                          | Regular app window (`xdg_toplevel`)      |
| **Panel**  | GNOME Shell, KDE Plasma, XFCE        | **Persistent chrome — bars, docks, taskbar** (a layer-shell surface) |
| **Layer**  | wlroots, sway, niri                  | A `zwlr_layer_surface_v1` — bars/wallpaper/notifications |
| **Popup**  | every shell                          | Transient surface (`xdg_popup`)          |
| **Pane**   | tiling shells, IDEs                  | A subdivision *inside* a window          |
| **Tile**   | tiling shells, COSMIC                | A window placed by a tiling layout       |

The collision is at **Panel**. In Cosmix's current phrasing, a "Panel" is
the generic top-level addressable surface — what the user-space window
*is*. In Wayland-shell phrasing across GNOME / KDE / wlroots-derived
compositors, a "Panel" is specifically a layer-shell chrome surface — the
top bar, the taskbar, the dock. Calling our top-level surface a "Panel"
will read to anyone with a Wayland-shell background as "this is a
layer-shell bar" — which it is not.

`xdg-decoration-unstable-v1` covers the SSD-vs-CSD question
(server-side vs. client-side decorations). KWin and wlroots compositors
honour it; GNOME refuses SSD entirely. "Always on top" / pin / minimise
are **not in core xdg-shell** — they exist only in compositor-private
extensions (`kde-plasma-window-management`, GNOME Shell DBus). Vocabulary
proposing those properties as universal is implicitly proposing a
compositor-private surface.

## Proposed vocabulary

Wayland precedence and AmigaOS lineage converge on the same shape — both
agree there is no `Panel` tier between display and window. Adopt:

| Cosmix term | Wayland mapping              | AmigaOS analog | Role                                                           |
|-------------|------------------------------|----------------|----------------------------------------------------------------|
| **Output**  | `wl_output`                  | Screen (closest) | Physical monitor; addressable but rarely directly bound to a Cosmix surface |
| **Workspace** | compositor-private (n/a)   | Screen (Z-stacked) | Logical grouping of Windows, Cosmix-owned policy not Wayland's |
| **Window**  | `xdg_toplevel`               | Window           | Regular app surface; the default top-level addressable container |
| **Pane**    | `wl_subsurface` (technical mapping; conceptual subdivision inside a Window) | Gadget-group | A subdivision inside a Window; the unit IDE-style apps split on |
| **Layer**   | `zwlr_layer_surface_v1`      | (none)           | Persistent chrome — bars, docks, wallpaper, notifications, lockscreen |
| **Popup**   | `xdg_popup`                  | Requester        | Transient overlay — menus, tooltips, autocomplete              |
| **Widget**  | (none — content)             | Gadget           | Interactive element inside a Window/Pane/Layer/Popup           |

`Panel` is retired as a generic Cosmix term. Where the current code
uses `Panel` to mean "top-level addressable surface," the new word is
`Window`. Where a downstream operator wants a top-bar or dock, they
declare a `Layer` and the divergence from `Window` is explicit at the
type level rather than implicit in props.

`Output` and `Workspace` exist in the vocabulary but are *implicit roots*
in most ui.* commands — a Window is created against an Output and placed
in a Workspace by compositor policy, not by ABP-level addressing in the
common case. Direct addressing of an Output (multi-monitor placement)
or a Workspace (cross-workspace move) is available but rare; this mirrors
Wayland, where outputs are queryable but not the surface authors target
in their primary commands.

## Decoration and window-property vocabulary

A second collision, smaller but worth flagging: the current `Decorations`
enum and `Layer` enum on `PanelProps` carry properties (`pin` /
`always-on-top`, `minimise`, etc.) that are neither in `xdg-shell` nor
in Intuition. These properties exist in compositor-private extensions
(KDE / GNOME / Hyprland) but are not portable, and Intuition's depth-gadget
model is closer to "send to back" than "pin to top."

Recommendation for the v0.2.x amendment that adopts this vocabulary:

- Drop `pin` / `always-on-top` from the cross-backend conformance
  contract. Move it to a per-compositor extension namespace (e.g.
  `wlroots.always-on-top: true`) that backends opt into.
- Replace `minimise` with `hide` (Wayland has no protocol-level
  minimise — `set_minimized` exists in `xdg-toplevel` v5+ but is a
  hint, not a guarantee). Cosmix can emulate via Workspace policy.
- Treat decorations (titlebar / borders / shadow) as a backend-negotiated
  property exactly the way `xdg-decoration-unstable-v1` does — request
  `client_side` or `server_side`, accept the compositor's response.

These are recommendations for the amendment, not commitments. They
require a separate operator pass before they bind backends.

## What changes in the 01b spec (proposed v0.2.x)

A future amendment of `_spec/2026-04-27-01b-amp-ui-vocabulary.md` that adopts this
memo would:

1. Rename the top-level command family from `ui.panel` to `ui.window`.
   Add a separate `ui.layer` for chrome surfaces. `ui.panel` remains
   as a deprecated alias for one minor release (additive, per §9 of
   the 01b draft) and is removed at v1.0.0.
2. Rename the `WidgetType::Panel` registry entry. The fenced-code
   widget-block convention is unaffected — only the noun changes.
3. Add `Output` and `Workspace` as queryable roots (read-only at v0.2.x;
   write semantics deferred until a real cross-workspace use case
   appears).
4. Split current `PanelProps` into `WindowProps` (xdg_toplevel-style)
   and `LayerProps` (layer-shell-style). The split is type-level, not
   prop-level — a backend that doesn't honour layer-shell can reject
   `ui.layer` cleanly without having to interpret a `layer:` field on
   a generic surface.
5. Apply the decoration / property recommendations from the previous
   section.
6. **No constitutional change.** The mandate's three criteria
   (legibility / modifiability / reconstructibility) are the reason to
   make the rename, not a casualty of it: `Window` is more legible to
   any operator (human or agent) coming from Wayland-shell context than
   `Panel`, and the `Layer` split makes layer-shell surfaces
   discoverable rather than papered over.

The amendment cost is moderate — `Panel` appears in roughly 30
identifiers across `cosmix-lib-display` and the display backend, plus
the spec text — and well-bounded. The cost of *not* renaming is permanent
vocabulary friction with every Wayland-shell-literate contributor and
every shell-adjacent project Cosmix might integrate with.

## What this memo does not do

- It does not ratify the rename. Operator review of the 01b draft is
  the gate per the discipline rule "spec first, code follows."
- It does not specify the migration path for already-deployed Mix
  scripts that send `ui.panel` commands. The deprecated-alias window
  in §9 of 01b covers the protocol side; whether scripts get a `mix
  fmt`-style auto-rewrite is a separate question.
- It does not address widget-implementation work in the display
  backend. The vocabulary change is at the ABP-protocol surface;
  widget rendering is downstream and follows mechanically.
- It does not propose moving any compositor capability into Cosmix.
  Cosmix remains a client of whatever Wayland compositor is running;
  this memo is about how Cosmix *describes* its surfaces, not about
  how those surfaces are arranged by the compositor.

## Why this is the right shape

The mandate frames every architectural decision against legibility,
modifiability, and reconstructibility by agents. The proposed
vocabulary advances all three:

- **Legibility** — every noun in the proposed vocabulary is a noun
  agents already know from Wayland documentation, wlroots source,
  and shell-extension ecosystems. No new vocabulary is invented; the
  agent-readable surface area shrinks.
- **Modifiability** — the type-level Window-vs-Layer split moves
  chrome-vs-content from a runtime prop to a static type. A backend
  that supports only `Window` (no layer-shell) can reject `Layer`
  cleanly; an autoresearch loop can target each separately.
- **Reconstructibility** — `Output / Window / Pane / Layer / Popup`
  is the same vocabulary a future Cosmix display rebuild against any
  Wayland compositor (or a non-Wayland substrate that honours the
  same primitives — Cocoa NSWindow / NSPopover / NSStatusBar map
  cleanly) could target without a vocabulary translation step.

Where AmigaOS and Wayland disagree, Wayland wins per the operator's
directive. Where they agree — the absence of a `Panel` tier between
the display and the window — the convergence is itself the strongest
evidence the rename is the right move.

## Status

**Decided 2026-04-26.** Operator approved the full proposal — the
five-noun vocabulary (`Output / Window / Pane / Layer / Popup`), the
decoration / window-property recommendations, and the Wayland-precedence
rule for window properties where Wayland and AmigaOS diverge. The memo
is now the authoritative input for the queued v0.2.x amendment of
`_spec/2026-04-27-01b-amp-ui-vocabulary.md`; no spec text or code is changed by
this decision until that amendment is drafted and ratified.

Adoption sequence stands: ratify 01b at v0.1.0 first (current draft,
vocabulary unchanged); then the separate v0.2.x amendment performs the
rename. Splitting the steps keeps the descriptive 01b chapter from being
held hostage to the rename work and keeps each ratification small.

**Wayland precedence rule (binding for the v0.2.x amendment):** where
AmigaOS Intuition vocabulary and Wayland window-property semantics
diverge — minimise, always-on-top / pin, decoration ownership, focus
stealing — Wayland's protocol-level vocabulary wins. AmigaOS lineage
informs *ergonomics* (port-per-application addressability, agent-
operability) but not *window-property semantics*.

The Wayland-research subagent's full transcript and the AmigaOS-research
subagent's full transcript are in this conversation's session log; the
load-bearing claims are summarised above.
