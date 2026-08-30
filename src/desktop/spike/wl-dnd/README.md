# Wayland DnD spike

Throwaway Bevy 0.19 / SCTK 0.19.2 transport probe. It attaches SCTK's
`data_device_manager` to winit's existing Wayland connection and surface; it
does not open a second Wayland connection.

## Build and run

From the repo's `desktop/` workspace:

```sh
cargo build -p spike-wl-dnd --release
env WAYLAND_DISPLAY=wayland-0 \
  XDG_RUNTIME_DIR=/run/user/1002 \
  WLDND_MODE=all \
  cargo run -p spike-wl-dnd --release
```

`WLDND_MODE` is `all` by default. Accepted values are `receive`/`recv`,
`send`, `echo`, and `all`. `echo` enables both directions. Set
`WLDND_SEND_DELAY_MS=1500` to delay `start_drag` after the pointer crosses
the 30 px threshold. `WLDND_REACTIVE_MS` controls Bevy's reactive event-loop
deadline and defaults to 1000 ms.

The source payload is written to `/tmp/wl-dnd-spike-payload.txt` immediately
before a drag starts.

## Log grammar

Each protocol observation is one line:

```text
WLDND PROBE1 connect_ok globals=<count>
WLDND PROBE1 seat=<name> caps=<keyboard|pointer|touch|none>
WLDND PROBE2 reactive_ms=<milliseconds>
WLDND PROBE3 teardown_ok
WLDND PUMP dispatched=<events in the last second>
WLDND SCALE factor=<factor> surface=(<x>,<y>) logical=(<x>,<y>)
WLDND RECV enter x=<x> y=<y> mimes=[...] source_actions=<actions>
WLDND RECV motion x=<x> y=<y>
WLDND RECV source_actions=<actions>
WLDND RECV action=<action>
WLDND RECV drop payload=<first 200 characters, escaped>
WLDND RECV leave predrop
WLDND RECV leave postdrop
WLDND SEND started serial=<serial> age_ms=<milliseconds>
WLDND SEND target_accepts=<mime|none>
WLDND SEND send_request mime=<mime>
WLDND SEND action=<action>
WLDND SEND dnd_drop_performed
WLDND SEND dnd_finished
WLDND SEND cancelled
WLDND SEND probe7 serial_stale suspected
WLDND ECHO own_drag_entered mimes=[...]
WLDND ABORT not_wayland
WLDND ERR <context>=<single-line error>
```

## Probe map

| Probe | Evidence |
|---|---|
| 1 — shared connection, globals, seat | `PROBE1 connect_ok`, `PROBE1 seat` |
| 2 — guest queue stays live in reactive mode | `PROBE2 reactive_ms`, regular `PUMP`, prompt `RECV`/`SEND` events |
| 3 — lifetime, teardown, scaling | `PROBE3 teardown_ok`, `SCALE` |
| 4 — receive and payload pipe | `RECV enter`, `motion`, `drop payload`, `leave` |
| 5 — live acceptance/actions | `RECV source_actions`, `RECV action`; left half accepts, right half rejects |
| 6 — source drag and lazy send | `SEND started`, `target_accepts`, `send_request`, `dnd_drop_performed`, `dnd_finished` |
| 7 — delayed mid-gesture serial | compare `SEND started ... age_ms`; test delay 0 and 1500; stale silence becomes `serial_stale suspected` |
| 8 — own-window echo | `ECHO own_drag_entered` followed by normal `RECV` lines |
