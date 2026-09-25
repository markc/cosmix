# term — the lightweight terminal

CosMix Term (`apps/term`, binary `term`) is the tabbed Mix terminal on iced
and wgpu. It is the lightweight frontend over `cosmix-term-core`, the same
toolkit-free core as the Bevy frontend CosMix BTerm (`bterm`). The two share
the PTY, the VT grid, the tab and pane model, the glyph raster and the Bus
handlers. Only the window differs.

It registers the Bus name `term` and serves the `term.*` verbs. bterm
registers `bterm` and serves `bterm.*`, so both can run at once. `mix --gui`
still prefers bterm until term reaches full verb parity (decisions D6 and
D10). Set `COSMIX_TERM_BIN` to the `term` binary to launch this one.

## Tabs and panes

The tab strip across the top shows one button per tab and a `+` that opens a
new one. Clicking a tab selects it. Each tab holds a tree of split panes, and
clicking a pane focuses it. When a tab has more than one pane, the focused
pane's border takes the design's focus-ring colour. Every other border,
including a tab's only pane, takes the plain border colour.
All strip and border colours come from the `cosmix-design` tokens.

Each tab shows its focused pane's program-set OSC title, falling back to `mix`.
A user-pinned title overrides OSC updates until cleared. The strip does not scroll or wrap. At the default window width, past about 17 tabs the `+`
and the later tabs run off the right edge, although up to 32 terminals can be
open. Ctrl+PageUp and Ctrl+PageDown still reach every tab.

Every pane keeps a border of the same width whether or not it is focused, so
moving focus never resizes a PTY. Split edges are placed on whole physical
pixels, so a pane's grid is never drawn half a pixel off the display.

Only the active tab's panes are drawn. A hidden tab keeps running and holds
no grid buffers. When it is shown again every pane repaints in full.

When a pane's shell exits, the pane closes. Closing the last pane of a tab
closes the tab, and closing the last tab quits. A shell that exits on its own
raises a desktop notification, as in bterm. Set `TERM_NOTIFY=0` to turn that
off.

## Keys

The chords are bterm's. They need Ctrl and Shift, or Ctrl alone where shown.
Any chord that also holds Alt or Super is left alone.

| Keys | Action |
|---|---|
| Ctrl+Shift+T | new tab |
| Ctrl+Shift+W | close the active tab |
| Ctrl+PageDown / Ctrl+PageUp | next / previous tab |
| Ctrl+Shift+E | split the focused pane side by side |
| Ctrl+Shift+O | split the focused pane top and bottom |
| Ctrl+Shift+X | close the focused pane |
| Ctrl+Shift+arrows | focus the nearest pane in that direction |
| Ctrl+Shift+Q | quit |

Held, these chords do not repeat. A held Ctrl+Shift+T opens one tab, and
the repeats are dropped rather than sent to the shell.

Everything else goes to the focused pane's shell. That means printable text
in any keyboard layout, Enter, Backspace, Tab, Escape, the arrows, Home, End,
Delete, PageUp, PageDown and Ctrl+A through Ctrl+Z.

## Font size

Font sizing follows foot. The window keeps its size and the grid gains or
loses cells. Every pane is re-rasterised, and every shell is told its new
size.

| Keys | Action |
|---|---|
| Ctrl+plus, Ctrl+equal, Ctrl+keypad plus | one step larger |
| Ctrl+minus, Ctrl+keypad minus | one step smaller |
| Ctrl+0, Ctrl+keypad 0 | back to the configured size |
| Ctrl+wheel up / down | one step larger / smaller per notch |

A step is foot's half point, which is two thirds of a logical pixel. The
size stays within the 6 to 48 pixel range that `font_px` accepts, and
stepping stops at either end. These chords repeat when held. Touchpad
scrolling counts 40 logical pixels of travel as one step.

The zoomed size lasts for the life of the window. It applies to every tab and
pane, including ones opened later. It survives moving the window to an output
with a different scale. It is not written back to `term.conf.mix`, so a new
window starts at the configured size.

## Configuration

`~/.config/cosmix/term.conf.mix` is shared with bterm. It sets `font_px`,
`scrollback` and `cursor`. `TERM_FONT_PX` overrides `font_px` for one run,
and `TERM_SPIKE_FONT` names a monospace font file.

- `term --print-config` prints the resolved settings and exits.
- `term --version` prints the version and build hash and does nothing else.

## Bus

term serves the core's `term.*` surface under the mesh-open law, like
bterm. `send term HELP` lists the full surface. A verb and the matching chord
make the same change, and the window follows a verb straight away. Every body
is a JSON object, and `{}` means no arguments.

| Verb | Body | Effect |
|---|---|---|
| `term.tabs` | `{}` | list tabs: id, active, title, cols, rows, child pid, revision |
| `term.tab.new` | `{"cwd":"/absolute/directory","title":"build"}` (both optional) | open and select a tab; explicit cwd must exist, be searchable by the current user and never falls back; title pins the tab label |
| `term.tab.title` | `{"id":N,"title":"build"}` | pin a label; empty string clears the pin and restores the focused pane's program-set OSC title (default `mix`) |
| `term.tab.move` | `{"id":N,"index":0}` | reorder to a zero-based index, clamped to 0–(tab count − 1); preserve selected tab and pane |
| `term.tab.select` | `{"id":N}` | select tab N |
| `term.tab.close` | `{"id":N}` | close tab N; closing the last tab quits |
| `term.panes` | `{"tab":N}` (optional) | list that tab's panes, default active tab: id, focus within the tab, cols, rows, child pid, geometry, tab, revision |
| `term.pane.split` | `{"dir":"v"}` or `{"dir":"h"}` | split the focused pane side by side (`v`) or top and bottom (`h`) |
| `term.pane.select` | `{"id":N}` | focus pane N in the active tab |
| `term.pane.close` | `{}` | close the focused pane; the last pane closes the tab |
| `term.snapshot` | `{"pane":N,"tab":T,"contents":true,"scrollback_lines":100}` (all optional) | read a pane anywhere; default is the focused pane in the selected/active tab; `contents` defaults true; history defaults 0, accepts 0–10000, capped at history above the current viewport |
| `term.type` | `{"pane":N,"text":"..."}` (`pane` optional) | type ASCII as keys into that pane, default focused pane; does not change focus |
| `term.props.watch` | `{}` | subscribe to the change topics through noded first, then enable publishing with this verb (returns JSON `{topics,revision}`), then read state |

```mix
send term term.tab.new
send term term.pane.split dir=v
send term term.panes
send term term.tab.new cwd="/tmp" title="build"
send term term.tab.title id=1 title="logs"
send term term.tab.move id=1 index=0
send term term.panes tab=1
send term term.snapshot pane=1 contents=true scrollback_lines=100
send term term.type pane=1 text="pwd\n"
send term term.props.watch
```

The same surface is served by bterm as `bterm.*`. The target-bound native
session lane is unchanged. Explicit stale tab/pane IDs return a `not-found`
error, never the active pane. With both snapshot selectors, the pane must
belong to the tab or the call returns `invalid-argument`. `contents:false`
returns the usual metadata and diagnostic timings without the screen marker
or text. Scrollback is prepended after the screen marker, oldest first;
`rows` still describes the viewport. Reading never moves the scroll offset.
Snapshot text represents empty grid cells as spaces, preserving column positions
and trailing blank cells in both history and viewport rows. Text is limited to
512 KiB of encoded bytes (including allowance for JSON escaping), leaving
metadata and transport headroom below the MCP 1 MiB and Bus 8 MiB limits.
The budget keeps the newest complete rows: the viewport first, then as much
recent history as fits. Retained rows are returned oldest first; the header reports
`truncated=true` when the byte budget omits rows and `lines_returned=N`
counts history and viewport rows actually returned. A row larger than the
budget returns no text rows. With `contents:false`, the count is zero and
`truncated=false`. Capture copies bounded rows under the grid lock; text
formatting runs after releasing the grid, terminal and tab-set locks.

Pinned and OSC titles have control characters and Unicode line/paragraph
separators stripped and are capped at 256 UTF-8 bytes on a character boundary.
An empty sanitised pin clears it. Titles may contain spaces; tab-list readers
should delimit the title at the final ` cols=` field, rather than tokenising
it on whitespace. Effective title changes advance the separate event revision
via `tabs.changed` and `title.changed` with `kind=retitled`; they leave the
tab-set and pane-layout revisions unchanged, so pane geometry stays valid.
Completion notifications use the same sanitised, possibly program-set label.
Explicit cwd paths follow symlinks and resolve `..` normally.

Existing replies keep their key=value format. New title replies are
`retitled id=N tab=N pane=P revision=R`; move replies are
`moved id=N index=I tab=N pane=P revision=R`. Tab creation still replies
`opened id=N tab=N pane=P revision=R binding=...`. All mutations, including
title/move and pane-selected typing, accept `request_id`; a retry with the
same verb and arguments replays the original success or refusal. Different
arguments with the same ID are a conflict. The JSON envelope limit remains
8192 bytes. Index must be an integer; invalid optional argument
types are refused.

Watch publishes `<service>.tabs.changed` (added, removed, moved, selected,
retitled), `<service>.pane.changed` (added, removed, selected, resized) and
`<service>.title.changed` (retitled). Each body is
`{tab,pane,kind,revision}`; the embedded Bus command is the topic suffix.
Events cover local UI actions and Bus mutations. OSC updates wake the core;
multiple updates before it handles the wake may coalesce to the latest title.
Pinned titles hide OSC updates until cleared. Unchanged values and replayed
mutations produce no new events. There is no timer or caller identity lease.

The watch revision is a separate monotonic event sequence, preserving the
older layout revision in verb replies. Publishing uses a bounded 256-record
queue and a serial, best-effort Bus sender. Subscribe before enabling watch,
then read current state; on a revision gap or reconnect read state again.
The watch response gives the current event revision. Events are not retained
by noded; publication failures are logged at most once per 30 seconds and
do not block terminal input. A dropped final event may remain undetectable
until a later revision arrives; read state whenever freshness is required.

`term.panes` reports each pane's geometry in logical pixels, relative to the
pane area below the tab strip, as bterm does. Geometry is from the last
frontend layout: hidden tabs can report stale values after a window resize,
or zeros after geometry invalidation. Select the tab and allow a frontend
layout before relying on its geometry. Grid dimensions describe the current
PTY grid and do not promise an up-to-date window layout.

With no broker, term prints that the Bus is unavailable and works as a
plain terminal.

## Not yet in this frontend

- Mouse reporting, wheel scrollback and selection. bterm has them, see
  [term-mouse](term-mouse.md).
- The app-control port: `app.describe`, `app.quit` and `app.controls.*` are
  not served yet.
- The native-session lane, see [term-native-session](term-native-session.md).
  bterm is still its only client.
- A verb for the font size. Until one exists, font size is set from the
  keyboard or the wheel only.
