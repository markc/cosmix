# Term selection and clipboard

The iced terminal uses iced 0.14's Wayland clipboard tasks, shared by its
tiny-skia and wgpu builds. No broker or clipboard daemon is needed.

| Input | Behaviour |
|---|---|
| Left drag | Select text in the pointed pane |
| Double / triple left click | Select a word / logical line, including wrapped rows |
| Single left click without dragging | Clear the selection |
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
focus, hiding or closing the original pane, or receiving a new press before
the old release ends the old gesture. Application presses get a release at
the original pane's last known cell, with the original modifiers. Cancelled
local drags clear their selection. Starting a new local selection clears
selections in all other panes, so only one highlight remains.

Each Terminal stores Rio's `Selection` in its own `Crosswords` grid. Pointer
rows map to signed grid rows as `viewport_row - display_offset`, under the
grid lock. Moving the viewport does not change the selection; Rio rotates or
invalidates it on output scrolling, erasure, resize and screen swaps. Evicted
history is clipped or becomes unselectable rather than selecting reused rows.
Rio's extraction joins soft wraps and trims trailing blanks, preserving
internal spaces; line selections include a final newline. Its semantic
selection uses Rio's word boundaries and matching-bracket expansion.
Copy includes the complete Rio cell text: combining marks, variation selectors,
ZWJ sequences, flag partners and skin tones. Selecting only the trailing half
of a wide cluster copies that cluster once. The local extraction adapter repairs
Rio `932c1a7`'s wrapped `LeadingSpacer` boundary by extracting the next row's
complete lead cell; it never edits or vendors Rio. A blank row ending at
that boundary does not gain a spurious newline, even when earlier rows are
selected. Rectangular copy includes a wrapped cluster once when the next
selected row starts on either its lead or its trailing spacer.

`Terminal::capture` swaps each selected cell's resolved foreground/background
after applying bold and inverse attributes. Selecting a wide character's
lead or trailing cell highlights both halves, matching copied text.
It compares the previous consuming
capture's range with the current range and dirties their visible rows. This
also catches parser-driven selection changes. Both painters consume the same
Screen and row mask; raster code and renderer damage tracking are unchanged.

Clipboard reads carry the target pane ID until completion. Focus/tab changes
cannot redirect a pending paste, and closing its pane discards the answer.
Successful nonempty pastes return that pane to the live bottom. With DECSET
2004, the human paste path adds bracketed-paste delimiters and removes both
embedded delimiter forms, including nested forms exposed by removal, then
strips ESC and ETX (`^C`) from the payload, as Rio's frontend does. Without
2004, CRLF and LF become CR. Unicode bytes are preserved. Human clipboard
payloads up to 16 MiB are admitted independently of the control queue; the
bracket delimiters do not count towards that payload limit. Larger pastes
send nothing and show a short error in the tab strip as well as logging it.
The notice clears after a successful paste. Empty pastes do nothing.

Human paste uses one `Msg::Input` in the existing metered FIFO and revokes
delegated control writers. It is never chunked: later keys and VT replies
follow the closing delimiter. Only control/Bus input (including permitted
writes and legacy diagnostic input) consumes the separate 64 KiB queue budget.
Local keys, local `type_text` and VT replies are exempt from that budget, so a
draining paste cannot refuse them. Paste entries have no key timestamp and
do not contribute to key-latency samples. Bus synthetic input continues through
its restricted encoder; it cannot open bracketed paste.
Human keyboard text and IME commits also preserve complete UTF-8 sequences,
including ZWJ and VS16; C0/C1 controls are filtered from text. Named keys and
Ctrl chords retain their existing encodings. Each seat event is queued as one
human FIFO entry, so replies cannot split an emoji sequence. The widget handles
iced IME events directly, enables the terminal input-method purpose, and lets
the runtime display preedit near the last captured caret. Preedit is never sent
to the shell; only a commit is. Composition records its owner pane. Changing
pane/tab focus, closing that pane or losing window focus clears preedit and
disables IME until the runtime acknowledges the reset. A commit is accepted
only for the active owner, so queued commits cannot reach another pane.
The Bus `encode_text` contract remains ASCII-only.
PRIMARY requires compositor primary-selection
support; missing clipboard offers produce no input. Clipboard writes have no
success acknowledgement in iced, so end-to-end Wayland delivery still needs a
live compositor check.

Headless regressions cover text extraction, wraps, history rotation/eviction,
capture colours/damage, paste encoding/admission, a 1 MiB paste delivered
through Rio's FIFO followed by a key and a VT reply, control queue limits,
chord precedence, cancelled gestures and click counting. See also the
[key and scrollback reference](../cos/term.md).

## Known limits

These are deliberately deferred, rather than part of this fix pass:

- Dragging at a pane edge does not autoscroll.
- Upstream Rio extraction at revision `932c1a7` adds a spurious newline for
  a blank soft-wrapped row outside the repaired wide-cluster boundary.
- Output overwriting selected cells does not invalidate the selection
  (Rio behaviour).
- Triple-click line selection copies a trailing newline (Alacritty/Rio
  convention).
- Unbracketed pastes pass C0 controls through (foot/Alacritty convention),
  apart from the documented newline-to-CR conversion.

The Unicode regressions cover byte-at-a-time parser input, capture and copy of
`👩‍💻`, `🇦🇺`, `👍🏽`, `❤️`, `é` and a space with a combining mark, trailing-half
and wrapped selections, scrollback, UTF-8 bracketed paste with ESC/ETX stripping,
keyboard queue atomicity and IME preedit/commit. The cluster runs these gates.
Rendering, font and resource limits are in
[the Unicode rendering section](term-foot-tactics.md#unicode-clusters-and-colour-emoji).
