# cosmix-powerd — battery and power state

**`cosmix-powerd` is the event-driven UPower citizen for battery, charging and
power-source state.** It turns the local system D-Bus view into a read-only
`power.*` Bus namespace and small lifecycle topics.

## What it does

- Reads UPower's virtual `DisplayDevice` and all devices returned by
  `EnumerateDevices`.
- Exposes presence, percentage, charging state, time-to-empty/full, energy rate
  and battery health when UPower provides them.
- Watches `PropertiesChanged`, `DeviceAdded`, `DeviceRemoved` and
  `org.freedesktop.DBus.NameOwnerChanged`; ordinary updates never depend on a
  polling loop.
- Keeps serving the Bus namespace when UPower is absent. The snapshot then has
  `present=false`. The daemon requests D-Bus activation once, waits for the
  owner edge without retry-spinning, and attaches when UPower claims its name.

The signal matches are installed before the first owner query and snapshot so
changes during startup remain queued. The tracker task drains the bounded raw
signal stream alongside every D-Bus method reply, including activation and the
final owner check, so a signal burst cannot back-pressure the shared D-Bus
reader ahead of that reply. If a signal lands around a multi-call scan or final
owner check, powerd discards that attempt and rescans. After three consecutively
raced attempts it publishes nothing, retains the previous complete snapshot,
logs the storm once, and uses the consumed signal wake to scan again. A
sustained storm can delay freshness but can never publish a mixed state. A
five-minute full rescan exists only as a missed-signal backstop.

## Running it

```sh
/opt/cosmix/bin/cosmix-powerd serve
```

The supplied `cosmix-powerd.service` is a systemd user unit because the Bus
citizen belongs to the graphical session. The SPEC-10 `cosmix-powerd` UID/GID
row reserves its daemon identity and namespace; it is not the runtime user of
this session service.

## Bus verbs

The service name is `power`.

| Verb | Purpose |
|---|---|
| `power.info` | Build identity, current power summary and cumulative publisher loss |
| `power.ping` | Liveness reply (`{"pong":true}`) |
| `power.props.get` | Full snapshot, or one path supplied as `args.path` |
| `power.props.list` | Current leaf paths |
| `power.props.describe` | Schema for one path supplied as `args.path` |
| `power.props.watch` | Topic discovery and subscribe-before-read bootstrap instructions |

## Property namespace

All properties are read-only. Optional numeric leaves are absent when UPower
reports that value as unknown.

| Path | Meaning |
|---|---|
| `present` | A system battery or UPS is present |
| `on_battery` | The system is currently running on battery |
| `battery.present` | Presence of the aggregate display battery |
| `battery.kind` | `battery`, `ups`, `line-power`, `peripheral` or `unknown` |
| `battery.power_supply` | The aggregate device supplies the system |
| `battery.percentage` | Remaining charge, 0–100 |
| `battery.state` | `unknown`, `charging`, `discharging`, `empty`, `fully-charged`, `pending-charge` or `pending-discharge` |
| `battery.time_to_empty_s` | Estimated seconds until empty |
| `battery.time_to_full_s` | Estimated seconds until full |
| `battery.energy_rate_w` | Watts; positive is discharging, negative is charging |
| `battery.health_percent` | Full-charge capacity as a percentage of design capacity |
| `devices.<id>.*` | The same fields for each enumerated UPower device |
| `lifecycle.props_level` | `L2` |
| `lifecycle.event_seq` | Daemon-session monotonic event sequence |
| `lifecycle.publisher_loss` | Cumulative publications lost during this daemon session |

`<id>` is a stable lowercase `d_<hex>` encoding of the complete UPower object
path. This fits the SPEC-07 segment grammar without collisions or leaking D-Bus
path syntax. Peripheral batteries are retained in `devices.*`; the root
`present` flag counts only a present `PowerSupply` battery or UPS.

## Topics

Domain-topic payloads contain `event`, `event_seq` and `data`;
`power.props.changed` uses the standard `{path,old,new,ts,cause}` shape plus an
`event_seq` header. Consumers subscribe on their Bus connection and then call
`power.props.get`; `event_seq` reveals an edge between that bootstrap snapshot
and later events.

Publication never back-pressures the UPower tracker. State topics coalesce
latest-wins by `(topic, path/device)`: property changes by property path,
battery changes by device ID, and power-source changes as one state key. The
keyed state map holds at most 1024 keys and drops the oldest key at the cap.
`power.device.added` and `power.device.removed` are events, so they share a
64-item FIFO and are never coalesced or reordered while loss-free.

Any publication loss — latest-wins replacement, keyed-state cap, event FIFO
overflow, stale owner epoch, broker send error or broker send timeout — advances
the cumulative daemon-session loss counter. The publisher then discards its
older backlog and publishes one `power.props.changed` control frame using the
same convention as interactd: `gap=true`, `lost_count`, and
`cause=publisher.loss`. On any frame with `gap=true`, a consumer must discard
its projected power state and re-read the complete snapshot via
`power.props.get` before applying later events. `lost_count` is the number of
discarded or failed publications covered by that gap; `power.info` and
`lifecycle.publisher_loss` expose the cumulative daemon-session total.

Loss diagnostics are episode-limited so a broker outage or state storm cannot
turn the synchronous reducer path into a logging bottleneck. The first loss
logs its cause; later losses only advance the counters. A successful publish
ends the episode and logs its total and duration. During a sustained outage, a
continuing summary is emitted no more than once per 60 seconds.

Every outgoing item is tagged with the unique UPower owner epoch. The publisher
checks that epoch immediately before its single send await and skips stale
items. It does not cancel an accepted send because cancellation cannot retract
a frame already accepted by the socket sink; at most one already-sent
old-epoch frame may still be in flight, and its `event_seq`/epoch turnover is
covered by the same gap-and-reseed contract.

| Topic | Published for |
|---|---|
| `power.props.changed` | Standard SPEC-07 leaf-level old/new property deltas |
| `power.battery.changed` | Aggregate or enumerated battery values changed |
| `power.device.added` | An enumerated UPower device appeared |
| `power.device.removed` | An enumerated UPower device disappeared |
| `power.on_battery.changed` | The system switched between mains and battery |

## Feature boundary

The default `cosmix` feature builds the UPower adapter and Bus citizen.
`cargo test -p cosmix-powerd --no-default-features` builds and tests only the
pure snapshot/diff core, without zbus, Tokio or Cosmix transport dependencies.
