# cosmix-noded — the Bus broker

**Every node in a cosmix mesh runs one `cosmix-noded`.** It is the WebSocket
message broker that Bus citizens register with and address each other through —
the piece that makes `send maild maild.stats` on one process reach a `cosmix-maild`
on another.

## What it is

A single binary running three async tasks:

- **the broker** — a WebSocket Bus router: services register a name, and it routes requests, replies, and topic pub/sub between them (local and cross-mesh).
- **the system monitor** — samples host CPU / memory / load and publishes it.
- **the Bus traffic logger** — taps every brokered message to an on-disk log.

It is the one daemon with no upstream dependency inside the mesh: a citizen needs a
broker before it can be addressed, so `cosmix-noded` starts first and everything
else orders `After=` it.

## What it does

- **Service registry.** A citizen sends `noded.register` on connect to claim its service name; the name is released on disconnect or `noded.deregister`.
- **Routing.** Requests to `<service>` route to the local holder; requests to `<service>.<node>.bus` route across the WireGuard mesh to that node's broker.
- **Topic pub/sub.** Citizens `subscribe` to topics; publishers fan out to every subscriber, with late-join replay where a topic supports it.
- **Admission control.** Under `mesh.admission=enforce`, a joining peer completes a challenge/response handshake (`noded.admit.challenge` / `noded.admit.response`) against the mesh authority before it can register.
- **Introspection.** `noded.list`, `noded.peers`, and `noded.inventory` report who is registered, which mesh peers are reachable, and each citizen's build provenance (binary, version, git sha, pid, start time).
- **Self-observation.** The monitor and logger each expose their own SPEC 12 property tree, so host metrics and log state are queryable as structured data.

## Running it

```sh
/opt/cosmix/bin/cosmix-noded serve
```

Normally run under systemd as `cosmix-noded.service`, started before every other
cosmix daemon. Shared node config is read from `/etc/cosmix/node.toml` (the
`[noded]` block sets the listen address and admission mode); `--listen`, `--node`,
and `--mesh-config` override at the command line. The broker binds its WebSocket
listener to the node's WireGuard interface — clients with no `node.toml` fall back
to `ws://127.0.0.1:4200/ws` (loopback).

## Interfaces

- **Transport:** WebSocket, path `/ws`, default port `4200` on the node's mesh address.
- **Broker verbs:** `noded.register`, `noded.deregister`, `noded.list`, `noded.peers`, `noded.inventory`, `noded.info`, `noded.ping`, `noded.tap` (the logger's message feed), and the admission pair `noded.admit.challenge` / `noded.admit.response`.
- **Monitor verbs:** `mon.status`, `mon.processes`, plus a `mon.props.*` property surface (`system.cpu_usage`, `system.mem_percent`, `system.load_avg_one`, …).
- **Logger:** taps `noded.tap`, writes `bus.log`, exposes a `log.props.*` surface (`runtime.events_seen`, `runtime.bytes_logged`, `lifecycle.tap_subscribed`, …).
- **Broker properties:** `noded.props.*` with change events on the `noded.props.changed` topic.

## Where it fits

- Links the Bus protocol family from the [bus](https://github.com/markc/cosmix) repo (`cosmix-lib-bus`, `cosmix-lib-client`) and the substrate libraries `cosmix-lib-config`, `cosmix-lib-mesh`, `cosmix-lib-mesh-trust`, `cosmix-lib-props-store`, and `cosmix-lib-log`.
- Every other daemon — `cosmix-maild`, `cosmix-webd`, `cosmix-dnsd`, `cosmix-indexd` — and every Mix `--serve` citizen is a client of it.
- Cross-mesh routing rides the WireGuard peering that `cosmix-lib-mesh` describes.

## See also

- [maild](maild.md) — a broker client: the JMAP-native mail daemon
- [libraries](libraries.md) — the substrate crates the broker links
- [overview](overview.md) — the cos daemon family
- [bus messaging](https://markc.github.io/mix/#_man/bus.md) — `send` / `emit` / `on` / `reply`, the primitives clients use to talk to the broker
- [serving as a citizen](https://markc.github.io/mix/#_man/serve.md) — writing a Mix service that registers with `cosmix-noded`
