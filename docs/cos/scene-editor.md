# Scene Editor

The Scene Editor chooses, installs, arranges and repairs the scenes on your
desktop edges: the panel, launcher, calendar, notifications and settings
pages. It is itself a scene. It is the shipped template `share/scenes/editor/`,
mounted by the same [scenes loader](scenes-loader) and rendered by the same
[Quoin](quoin) as every other page. Its behaviour is the Mix citizen
`scene-editor`. Everything it does is a `scenes.*` or `shell.*` Bus verb, so an
agent can do all of it without the UI, locally or over the mesh.

It opens as a centred 880×620 dialog titled *Scene Editor*. The dialog takes
the keyboard when it opens. Escape or the title bar's **×** hides it.

## Opening it

| Route | Sends | Opens |
| --- | --- | --- |
| **Ctrl+Alt+P** (left or right Ctrl, left Alt) | `scenes.editor.open {safe:true}` | the shipped copy (safe mode) |
| Corner menu → **Edit panels…** (every corner) | `scenes.editor.open {safe:true}` | the shipped copy (safe mode) |
| Launcher → **Scene Editor** | `scenes-cli.mix editor` → `scenes.editor.open {}` | your user copy if one validates, else the shipped copy |
| An agent | `scenes.editor.open {safe?, view?, scene?}` | as asked |

The chord and the corner item **toggle**: while the shipped editor is visible,
either one hides it. The chord is two inputd default rows on `KEY_P`, one with
`{left_ctrl, left_alt}` and one with `{right_ctrl, left_alt}`. Right Alt/AltGr
is left out on purpose, because it is level-3 shift on some layouts. The
chord needs inputd's keyboard grab, so it does not work on hosts without one
(desk containers, a nested harness); the corner menu and the launcher still
do.

**An upgraded host does not get the chord by itself.** inputd seeds its
default rows only into a missing or unusable keymap file; an existing keymap
is the user's and is served as it is. After upgrading to inputd 0.4.5, bind
both strokes with `input.bind` (the row is in
[daemon-help](daemon-help.md)), after checking `input.query` that neither
stroke is already bound to something else, because `input.bind` replaces a
row silently. Until then the corner menu's **Edit panels…** is the recovery
path.

`view` is `gallery`, `installed` or `arrange`, and `scene` preselects an
installed scene. An open increments the editor's `request_seq`, and the
behaviour applies `view`/`scene` from the loader's `editor` record. So
`scenes.editor.open {view:"installed", scene:"panel"}` from an agent navigates
the UI.

`scenes.editor.close {unload?}` hides it. With `unload:true` the loader also
stops its behaviour and unmounts it. Otherwise it stays mounted, hidden and
idle, for the loader's lifetime: while hidden it ignores inventory changes that
only bump revision or generation counters (every model publish of every
scene does), and rebuilds only for something it would show.

## Safe mode and the user copy

**Safe mode is the shipped copy**, read in place from `$SCENES_TEMPLATES/editor`
(root-owned on a system install), and never copied. The chord and the corner
item always open it, like a BIOS setup key: the recovery path never reads a
user file. An explicit open also revives an editor whose behaviour has
crash-looped or is backing off, instead of showing a tree with nothing behind
it. A `safe mode` badge shows in the tab row. Lint *warnings* in the
shipped behaviour never refuse it. They show as an amber banner ("the
editor's behaviour has N lint findings; it still runs"), because the one path
with no fallback must not break when a Mix upgrade adds a warning.

To change the editor itself:
1. Install a user copy with `scenes.install {template:"editor"}`. This creates
   `~/.config/cosmix/scenes/editor/`. It is never enabled; `enable:true` is
   refused `SCENES_RESERVED`.
2. Edit it in ced. Its helpers live in its own `lib.mix`, beside
   `behaviour.mix`.
3. Open the editor from the launcher, not the chord.

If the user copy fails the install bar (scene lint, citizen mismatch,
`mix --check`, or `mix lint --deny-warnings` on the behaviour), or fails to
mount, the shipped copy opens instead. A red banner names the reason:
`editor.fallback {error_code, message, file}` in `scenes.list`. Once the user
copy is mounted, a broken reload of it keeps the last good tree. While the
shipped copy is mounted, edits to the user copy change nothing until the next
non-safe open (`editor.user_dirty` records that they happened).

A hand-made `~/.config/cosmix/scenes/editor/` is treated as a user copy: it
is validated and falls back with its diagnostic. It is never silently ignored.

## The three tabs

**Gallery** lists the shipped templates from `scenes.templates`: title, the
one-line description, edge, and "installed as …". Its buttons are *Install*,
*Install and enable*, and the highlighted **Recommended set**. That set
installs and enables, in gallery order, every template with
`recommended:true` that is not already on, and stops at the first refusal.
If a template name is taken, *Install* retries once as `<template>-2`.
Templates whose `requires` services are not registered show "needs …".
Nothing is applied until you click.

**Installed** lists every installed scene with a status dot:
- green: mounted and running;
- amber: a diagnostic, or still starting/in backoff;
- red: crash loop, or refused and not mounted;
- grey: disabled.

Selecting a scene shows its edge, template and problems (at most 20 rows; ced
has the full list), and these buttons:
- *Enable/Disable*, *Reload*, *Reset to template*;
- *Remove*, which is two-step: the first click arms it ("Confirm remove"),
  and any other action disarms it. There is no timer;
- *Edit scene*, *Edit behaviour*;
- *Try in sandbox*, and *Promote* (offered on a sandbox only);
- *move to* left/top/bottom/right.

**Arrange** shows the four edges. Each has its pages in carousel order, the
mode buttons (hidden, pinned, docked) and a thickness stepper (−/+). Select
a page, then use ←/→ to swap it with its neighbour.

Action buttons answer at once with `{queued:true}`. The action runs in the
behaviour as one local task, one at a time; a second click while one runs
gets "Still working on the last change". The status line at the bottom says
what happened.

## Editing a scene in ced

*Edit scene* (or *Edit behaviour*) opens the file in [ced](ced) and hides the
dialog. If a `ced` service is registered, the behaviour sends
`ced.open {paths:[<absolute path>], line?}`, where `line` is the first
problem's line. Otherwise it sends
`apps.launch {id:"dev.cosmix.ced", uris:[<path>]}`. It never spawns ced
itself: a child of the behaviour would sit in the loader's cgroup and die with
a loader restart.

Save with `Ctrl+S` and the loader reloads the scene live. A broken save leaves
the old tree applied. ced shows its own in-process scene lint straight away.
After the save it also shows the loader's verdict: the editor pushes
`ced.diagnostics {path, source:"scenes", digest, diagnostics}` for every scene
file open in ced whose (problems, digest) pair changed. It sends `[]` when a
scene recovers. `digest` is the sha256 of the bytes the loader attempted, so
ced shows the set only while the tab's text matches them. The digest is part
of the change key, so a re-save that produces the same problems still
re-pushes. If ced was not running when *Edit* launched it, the push is sent
once ced registers. A verdict is counted as delivered only once ced has taken
it: changes that happen while ced is away, or a push ced refused, are sent
again when it is back.

## Sandbox: try, then promote

*Try in sandbox* (`scenes.fork {name}`) copies your installation as
`<name>-sandbox`, or `<name>-sb` when the longer name would break the Bus name
rule or is already taken. The copy is installed as another carousel page on the same edge and
enabled, so the original keeps working while you edit the copy. On a
single-page edge such as the bottom panel, selecting the sandbox page hides
the original until you page back.

*Promote* (`scenes.promote {from}`) validates the staged replacement, the
sandbox rewritten back to the original name and page, before it moves
anything. It then swaps the replacement in, and moves the old original and
the sandbox to `.recovery/`. A refusal moves nothing. A crash mid-swap is
finished or rolled back by the next loader start, decided by what is on disk
(see [the loader](scenes-loader#fork-promote-and-move)). A fork keeps its
original's template origin, so *Reset* works on it too.

## Arrange

- **Order**: one `shell.panel.order {edges:{<edge>:[…]}}` per change, written
  to Quoin's `conf.mix`.
- **Mode**: `shell.panel.mode {edge, mode}`.
- **Thickness**: `shell.panel.resize {edge, thickness_px}`. The steps are 4 px
  on top/bottom and 20 px on left/right, clamped to 24–200 and 120–500 and to
  the panel's output budget. At a bound it is a no-op with a status line.
- **Move to edge**: `scenes.move {name, edge}` rewrites only the scene's
  `window` header (dropping `w`/`h` across orientations). One
  `shell.panel.order` then names both edges, so the page leaves the old
  edge's declared list and joins the new one.

Pages shown per edge are the declared (`conf.mix`) order first, then any live
pages it does not name. An empty declared slot shows dimmed; move it past the
end to drop it. `shell.panel.order` re-encodes the whole `conf.mix`: other
values are kept, comments and formatting are not.

## First run

When the loader sees Quoin come live with no enabled scenes (`needs_setup`),
it opens the editor on the Gallery with `first_run:true`. This happens only
if the state file read cleanly. The Gallery then shows "Nothing is on your
desktop yet. Pick panels to add." and highlights the Recommended set. Hiding
it with Escape or × sets `dismissed`, so the same loader process does not
reopen it;
the next login does, while `needs_setup` holds. A desktop with any scene
enabled never auto-opens. `SCENES_FIRST_RUN=0` in the loader's environment
turns first run off; test harnesses set it.

## Verbs

All of these are mesh-open, with no authorization gate. Refusals are rc 10
with `{error_code, message, context?}`.

| Verb | Args → reply |
| --- | --- |
| `scenes.templates` | `{}` → `{root, templates:[{template, name, title, description, edge, kind, behaviour, recommended, order, requires, installed_as, diagnostic?}]}`, sorted by `order` then `template`; `hidden:true` templates are omitted |
| `scenes.editor.open` | `{safe?, view?, scene?, first_run?}` → `{name:"editor", source:"shipped"\|"user", safe, visible, pending, fallback, request_seq}` |
| `scenes.editor.close` | `{unload?}` → `{name:"editor", visible:false, mounted}` |
| `scenes.fork` | `{name, as?, enable?=true}` → `{name, installed:true, enabled, forked_from}` |
| `scenes.promote` | `{from}` → `{name, recovery:[paths], removed}` |
| `scenes.move` | `{name, edge}` → `{name, edge, revision}` |
| `shell.panel.order` | `{edges:{<edge>:[page]}}` → `{edges}` ([Quoin](quoin)) |
| `shell.dialog.show` / `shell.dialog.hide` | `{scene}` → `{scene, visible, applied}` ([Mix Scenes](scenes#dialog-scenes)) |
| `shell.scene.layout` | `{scene, node?}` → surface and node rects in logical px ([Mix Scenes](scenes#dialog-scenes)) |
| `editor.state` (on `scene-editor`) | `{}` → `{model, ui, busy, last, services}`; the published model, the selection, whether an action is running and the last outcome |

`scenes.list` carries the editor as `editor` (not as a row in `scenes`), plus
`state_ok`, per-scene `files`/`digests`/`problems`/`forked_from`. The loader
documents these in [the loader](scenes-loader#scene-editor).

Directed click verbs on `scene-editor` (`editor.view`, `editor.recommended`,
`editor.scene.*`, `editor.edge.*`, …) are UI glue for Quoin's clicks. They are
not a stable API; drive the `scenes.*`/`shell.*` verbs instead.

## Recovery

The editor is a dialog on Quoin, so a Quoin that never comes up means no
editor. The out-of-band path is `scenes-cli.mix`, from any terminal and over
the mesh (`--target scenes.<node>.bus`):

```sh
mix scenes-cli.mix list
mix scenes-cli.mix reset panel         # restore a scene from its template
mix scenes-cli.mix remove notes        # disable, unload, keep a copy in .recovery
mix scenes-cli.mix reset editor        # restore the editor's user copy from the shipped template
mix scenes-cli.mix editor --safe       # open the shipped editor
```

It is installed next to the loader: `$COSMIX/bin/scenes-cli.mix` on the dev
tier and `/opt/cosmix/share/desktop/scenes-cli.mix` on a system install. It
exits 0 on success, and otherwise prints `{error_code, message}` and exits 1.

A wedged editor behaviour can still be dismissed with **×**, which is Quoin
frame chrome, not a scene node. `scenes.remove {name:"editor"}` removes only
your user copy (moved to `.recovery/`); the shipped editor stays. With an
unreadable `state.conf.mix`, every mutating verb is refused
`SCENES_STATE_UNREADABLE` rather than overwrite the file. Safe mode still
opens, with a red banner naming the file.

## Limits (v1)

- No drag-and-drop, selection overlays on the live shell, node tree, property
  inspector, bindings editor or undo. Arrange is buttons.
- One dialog seat, and it is the editor's: other scenes may not use
  `window.kind:"dialog"` (`SCENES_RESERVED`).
- No template previews in the gallery, and no template sharing across the mesh.
- The editor's colours are Breeze Dark literals, like every shipped template;
  scene theme tokens are separate work.
- `.recovery/` is never pruned automatically. The status line names the
  recovery path after a reset, remove or promote.
- An enabled scene left unmounted by a double remount failure is retried only
  by *Reload*, the next file event or the next Quoin return; there is no timer.
- `mix lint` run directly on a scene file still reports a false `MIX-E1003`
  at the fence line. ced lints scenes in-process and is not affected.
