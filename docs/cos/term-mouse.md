# Terminal mouse input

CosMix Term forwards mouse input to applications that enable xterm mouse
tracking: X10 (9), click (1000), button motion (1002), and all motion (1003).
It supports normal, UTF-8 (1005), and SGR (1006) reports. Applications such as
Edit can enable `ESC[?1002;1006h` to receive clicks, drags and wheel input.

Reports use one-based cells in the pane's rendered grid. Split-pane offsets,
borders and fractional display scaling are accounted for. Left, middle and
right buttons map to 0, 1 and 2; Alt and Ctrl retain their xterm modifier bits.
Wheel movement sends one button 64/65 report per line. Pixel wheel movement
accumulates until it covers a cell height.

Shift bypasses application mouse reporting. The wheel then scrolls local
history. Without mouse tracking, the wheel uses alternate-screen cursor keys
when alternate scrolling is enabled, otherwise local history. Clicking still
focuses the pane. Mouse reporting does not add local text selection.

The encoder and mode gates are ported from Rio's MIT-licensed librio.
