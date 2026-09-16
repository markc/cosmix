# iced clipboard comparison port

`cosmix-clip-iced` is a floating 720×520 clipboard history client using
iced 0.14, wgpu and Tokio. It reads the broker from
`/etc/cosmix/node.conf.mix` and local/remote provider targets from
`/etc/cosmix/clipboard.conf.mix`. With no configuration it uses the
loopback broker on port 4200 and local provider `desktop-vt1`.

Typing filters previews and IDs locally; Enter queries full history on the
provider. Rows pick the live selection; remote rows fetch entry text over
the Bus and write it to the local provider. Clear requires a second click
within 2.5 seconds. Pause/Resume controls history collection.

The client registers a random `clippanel-` name and subscribes to the
provider's advertised topic. Refreshes are coalesced for 250 milliseconds;
there is no periodic UI or history timer. Ages update on redraw. Menu
events destroy the window or create a fresh window, while the Bus
subscription survives. Closing the window also leaves the listener alive.
The native Bus client accepts topic envelopes without a command header.

`CLIPPANEL_SMOKE=1` runs a headless refresh and writes the acceptance JSON
to `$XDG_RUNTIME_DIR/clippanel_smoke.out`. `CLIPPANEL_SMOKE=verbs` also
exercises pause on/off, search, newest-entry pick and menu, waiting up to
three seconds for its menu event. The overall deadline is 15 seconds;
timeout writes `TIMEOUT` and exits 1. No graphics connection is made in
smoke mode. Remote unavailability is reported and hides the remote section.

Build and clippy checks for this experiment run on a build worker with an
exact pushed commit. The live seed/hide/show/pick/read-back screenshot
exercise and idle measurements are separate orchestrator acceptance gates.
