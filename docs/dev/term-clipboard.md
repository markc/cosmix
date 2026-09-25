# Term selection and clipboard

The iced terminal uses iced 0.14's Wayland clipboard tasks, shared by its
tiny-skia and wgpu builds. No broker or clipboard daemon is needed.

| Input | Behaviour |
|---|---|
| Left drag | Select text in the pointed pane |
| Double / triple left click | Select a word / logical line, including wrapped rows |
| Single left click without dragging | Clear that pane's selection |
| Ctrl+Shift+C | Copy the focused pane's selection to CLIPBOARD |
| Ctrl+Shift+V or Shift+Insert | Paste CLIPBOARD into the focused pane |
| Selection release | Write the completed selection to PRIMARY |
| Middle click | Paste PRIMARY into the pointed pane |
| Shift+mouse | Override application mouse reporting for local selection/paste |

Copy/paste chords do not autorepeat. Letter chords use the existing
layout-aware table: the logical Latin letter wins, with physical C/V as a
fallback for non-Latin layouts. Alt/Super combinations are left alone.
Plain Ctrl+C still sends the interrupt character.

Clicks count within 300 ms in the same pane and cell; the fourth starts a
new single click. Drags use the left/right half of each cell as boundaries.
A drag stays attached to its original pane and clamps at its edges; there is
no edge-triggered autoscroll. Wheel/history keys can move the viewport while
selecting. Mouse ownership is chosen at button press: hold Shift before
pressing to start a local gesture in a mouse-reporting application. A matching
reported release is sent even if Shift changes before release. Losing window
focus drops the pending gesture.

Each Terminal stores Rio's `Selection` in its own `Crosswords` grid. Pointer
rows map to signed grid rows as `viewport_row - display_offset`, under the
grid lock. Moving the viewport does not change the selection; Rio rotates or
invalidates it on output scrolling, erasure, resize and screen swaps. Evicted
history is clipped or becomes unselectable rather than selecting reused rows.
Rio's extraction joins soft wraps and trims trailing blanks, preserving
internal spaces; line selections include a final newline. Its semantic
selection uses Rio's word boundaries and matching-bracket expansion.

`Terminal::capture` swaps each selected cell's resolved foreground/background
after applying bold and inverse attributes. It compares the previous consuming
capture's range with the current range and dirties their visible rows. This
also catches parser-driven selection changes. Both painters consume the same
Screen and row mask; raster code and renderer damage tracking are unchanged.

Clipboard reads carry the target pane ID until completion. Focus/tab changes
cannot redirect a pending paste, and closing its pane discards the answer.
Successful nonempty pastes return that pane to the live bottom. With DECSET
2004, the human paste path adds bracketed-paste delimiters and removes both
embedded delimiter forms, including nested forms exposed by removal. Without
2004, CRLF and LF become CR. Unicode bytes are preserved. The existing 64 KiB
PTY queue limit applies to the whole encoded paste: oversized/busy writes are
rejected and logged, not partially sent. Empty pastes do nothing.

Human paste uses the existing metered input queue and revokes delegated
control writers. Bus synthetic input continues through its restricted encoder;
it cannot open bracketed paste. PRIMARY requires compositor primary-selection
support; missing clipboard offers produce no input. Clipboard writes have no
success acknowledgement in iced, so end-to-end Wayland delivery still needs a
live compositor check.

Headless regressions cover text extraction, wraps, history rotation/eviction,
capture colours/damage, paste encoding/admission, chord precedence and click
counting. See also the [key and scrollback reference](../cos/term.md).
