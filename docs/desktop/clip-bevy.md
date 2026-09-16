# Clipboard panel (Bevy/CTK comparison port)

`cosmix-clip-bevy` opens a 720×520 clipboard history picker. Configuration
comes from `/etc/cosmix/node.conf.mix` (`wg_ip`, `noded.port`) and
`/etc/cosmix/clipboard.conf.mix` (`local`, `remote`). Without configuration,
the broker defaults to loopback port 4200 and the local target to `desktop-vt1`.

Search filters previews and IDs immediately; Enter queries full server history.
Click a local row to select it. Remote rows fetch the granted entry and write
its text to the local clipboard. Pause/Resume controls recording. Clear needs
a second click within 2.5 seconds. Topic menu events destroy/recreate the window;
other events coalesce into a refresh after 250 ms. Provider session expiry
refreshes capabilities and retries the affected verb once.

The registered, supervised native Bus client runs on a worker thread and wakes
Bevy when data arrives. The UI uses `WinitSettings::desktop_app()` without a
polling refresh timer. Row ages update when the view is redrawn.

Native client 0.6.3 preserves topic deliveries with or without a `command`
header. The panel dispatches by `topic`, not by `type: event`, so both Mix
publications and event-only envelopes reach the same subscription handler.

Set `CLIPPANEL_SMOKE=1` for one headless refresh, or `CLIPPANEL_SMOKE=verbs`
to also exercise pause on/off, server search, pick, menu and topic delivery.
Both write `$XDG_RUNTIME_DIR/clippanel_smoke.out` (the system temporary directory
if unset). An overall 15-second deadline writes `TIMEOUT` and exits 1.
The verbs smoke changes clipboard recording state and selects the newest entry;
run it in a test session. It never initialises a window or graphics device.
