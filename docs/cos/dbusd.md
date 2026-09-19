# cosmix-dbusd — the D-Bus boundary daemon

**`cosmix-dbusd` is the one cosmix process that speaks D-Bus.** Per the
ADR *D-Bus is a foreign protocol — only `cosmix-dbusd` adapters speak it*
(2026-09-18), every other cosmix app and daemon talks ABP on the Bus; the
D-Bus world is reached — in both directions — through per-domain adapters
hosted here, the same way IMAP and HTTP live at the edge.

## What it does

- Hosts one adapter per D-Bus domain. An adapter owns exactly one Bus
  service (registered under its domain name: `notify`, `tray`, …) and one
  zbus connection for its whole run — both locals of its `run`, so an
  adapter that returns, fails or panics drops them and withdraws its
  names. Fault containment falls out of ownership: one adapter's death
  never touches another adapter or the daemon. One language limit: a
  panic inside `Drop` during a panic unwind aborts the whole process —
  containment covers one fault, not a second one during cleanup.
- Supervises every adapter in its own tokio task, observing the
  JoinHandle: a panic (`JoinError::is_panic`), an `Err` return, or an
  unexpected `Ok` return all record a failure, put the adapter in
  `backoff`, and relaunch it after an exponential schedule (1 s doubling
  to a 60 s cap; five healthy minutes reset the escalation). A
  consistently failing adapter stays in `backoff` forever — it never
  takes the daemon down. A run that never yields cannot be preempted:
  it is reported `stuck`, its Bus service and D-Bus names may stay held
  until the process restarts, and its lifecycle keeps answering
  commands.
- Dials the desktop session bus from `DBUS_SESSION_BUS_ADDRESS` per
  adapter. Unset or unreachable means that adapter sits in `backoff`
  with that reason; the daemon stays up and keeps answering `dbusd.*`.
- Serves the `dbusd` Bus control service (mesh-open; no caller
  authorization on any verb):
  - `dbusd.adapters` — every adapter's `{name, service, state, restarts,
    last_error, since}` and the session-bus endpoint state.
  - `dbusd.adapter.restart {name}` / `dbusd.adapter.enable {name}` /
    `dbusd.adapter.disable {name}` — disable stops the adapter and
    releases its names (how a human hands, say, the Notifications name
    back to another daemon); enable and restart relaunch it. Unknown
    names get a refusal reply, never a panic.
  - `dbusd.props.{get,list,describe,watch}` — the uniform props surface
    over `dbusd.adapters.<name>.state|restarts|last_error`.
  - `dbusd.ping`, `dbusd.info`.
- Publishes state changes as they happen — one `dbusd.adapter.changed`
  event plus `dbusd.props.changed` diffs per transition, both stamped
  with a per-daemon-session monotonic `event_seq`. Events can be
  dropped under backlog: a gap in `event_seq` says so, and the props
  (`dbusd.props.get`) are the truth. No polling anywhere: supervision
  is JoinHandle- and signal-driven.

Configuration is `~/.config/cosmix/dbusd.conf.mix`
(`enabled: ["notify", "tray"]`); an absent `enabled` means every
built-in adapter, an empty list means none. An absent file materialises
the defaults on disk; a file that exists but cannot be read or parsed
is fatal — the daemon exits rather than silently guessing the adapter
set. Unknown names are logged and ignored. Enable/disable verbs are
runtime-only — the config file is the persistent source.

## Adapter status

| state | meaning |
|---|---|
| `starting` | launched; has not yet reported serving |
| `running` | serving its domain |
| `backoff` | failed (error/panic/exit); waiting out the schedule |
| `disabled` | stopped by config or verb; names released |
| `stuck` | the run ignored its stop signal and the abort; its names may stay held until the process restarts |

`restarts` counts every relaunch in this daemon process (backoff
restarts, the restart verb, enable-after-disable). `last_error` is the
most recent failure reason, cleared when the adapter runs healthy again.

## Running it

```sh
/opt/cosmix/bin/cosmix-dbusd serve
```

The supplied `cosmix-dbusd.service` is a systemd user unit: the Bus
citizen belongs to the graphical session, and the adapters dial the
desktop session bus (`unix:path=/run/cosmix-desk-dbus/bus`, set
explicitly because systemd user units do not inherit the session
environment). SIGTERM stops every adapter (releasing its D-Bus names)
and exits 0.

## Writing an adapter

An adapter implements the `Adapter` trait — `name()`, `bus_service()`,
and `async run(ctx)` — and registers as one line in
`citizen::builtin_adapters()`:

```rust,ignore
adapter_spec::<NotifyAdapter>(),
```

The context carries the session-bus endpoint, a per-launch stop signal,
a ready channel, and (under the `cosmix` feature) helpers that open the
adapter's own Bus connection and its own zbus connection. Keeping both
as locals of `run` is the whole containment story. The crate's core
(supervisor, backoff, registry, config) builds without the `cosmix`
feature and is unit-tested with a scripted fault adapter compiled only
under `cfg(test)`.

The first adapters (notify, tray, settings, then folding mprisd and
powerd) land in later jobs; J1 ships the host, the control service and
the supervision guarantees.
