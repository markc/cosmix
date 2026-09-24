# term — the lightweight terminal

CosMix Term (`apps/term`, binary `term`) is the tabbed Mix terminal on iced
and wgpu. It is the lightweight frontend over `cosmix-term-core`, the same
toolkit-free core as the Bevy frontend CosMix BTerm (`bterm`). The two share
the PTY, the VT grid, the tab and pane model, the glyph raster and the Bus
handlers. Only the window differs.

It registers the Bus name `term` and serves the `term.*` verbs. bterm
registers `bterm` and serves `bterm.*`, so both can run at once. `mix --gui`
still prefers bterm until term reaches full verb parity (TODO-term D6 and
D10). Set `COSMIX_TERM_BIN` to the `term` binary to launch this one.

## Tabs and panes

The tab strip across the top shows one button per tab and a `+` that opens a
new one. Clicking a tab selects it. Each tab holds a tree of split panes, and
clicking a pane focuses it. The focused pane's border takes the design's
focus-ring colour. Every other pane's border takes the plain border colour.
All strip and border colours come from the `cosmix-design` tokens.

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
| Ctrl+wheel forward / back | one step larger / smaller per notch |

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

term serves the core's `term.*` surface under the mesh-open law, like bterm:
`term.tabs`, `term.tab.new`, `term.tab.select`, `term.tab.close`,
`term.panes`, `term.pane.split`, `term.pane.select`, `term.pane.close`,
`term.snapshot` and `term.type`. `send term HELP` lists the full surface.
A verb and the matching chord make the same change, and the window follows a
verb straight away.

```mix
send term term.tab.new
send term term.pane.split dir=v
send term term.panes
```

`term.panes` reports each pane's geometry in logical pixels, relative to the
pane area below the tab strip, as bterm does.

With no broker, term prints that the Bus is unavailable and works as a
plain terminal.

## Not yet in this frontend

- Mouse reporting, wheel scrollback and selection. bterm has them, see
  [term-mouse](term-mouse.md).
- The menu bar and the About dialog.
- The native-session lane, see [term-native-session](term-native-session.md).
  bterm is still its only client.
- A verb for the font size. Until one exists, font size is set from the
  keyboard or the wheel only.
