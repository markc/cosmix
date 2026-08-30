# cosmix-dnsd — authoritative DNS for the mesh

**`cosmix-dnsd` is an authoritative DNS server for a WireGuard mesh: it answers
the mesh's internal names, with zones generated from the mesh inventory rather
than hand-edited zone files.** One source of truth (the node roster) compiles
to the A/AAAA/SOA/NS records the daemon serves.

## What it is

A small authoritative resolver that binds `:53` on the node's WireGuard mesh
address (plus a loopback `127.0.0.1:53` for the node's own stub resolver). It
serves the mesh's private naming — every node reachable by a stable name over
the WG interface — and refuses anything outside its authority. It is not a
recursive resolver and not a public nameserver.

Zones are **generated, never hand-written**. The mesh inventory (the roster of
nodes and their WG addresses) is the source of truth; a build step turns it
into a validated zone snapshot with real apex `SOA`/`NS` records. Editing zone
data means editing the inventory and regenerating — the daemon serves whatever
the snapshot says.

## What it does

- **Authoritative answers** for the mesh's internal zones over UDP and TCP on `:53`.
- **Inventory-driven zones** — records compiled from the node roster via the `cosmix-lib-dns` codec, so a node rename or renumber flows from one edit.
- **Serial discipline** — one owner drives the serial across all its zones; a mismatch fails closed rather than serving a split view.
- **Read-only Bus surface (v0)** — the zone snapshot and per-rcode response stats are queryable; mutation verbs are deliberately rejected in v0.

## Running it

```sh
/opt/cosmix/bin/cosmix-dnsd
```

Runs under systemd as `cosmix-dnsd.service` (identity `User=cosmix-dnsd`).
It reads the node's mesh IP from `/etc/cosmix/node.toml` and binds
`<wg-ip>:53`, retrying briefly on `AddrNotAvailable` so the unit stays
portable across nodes that bring the WG interface up asynchronously. Binding
`:53` needs `CAP_NET_BIND_SERVICE`. Standalone `--listen <ip:port>` flags are
available for testing without a live WG interface.

Example generated zone (illustrative — public placeholders only):

```text
; zone: example.com  (compiled from the mesh inventory)
example.com.        SOA   ns1.example.com. hostmaster.example.com. ( 42 ... )
example.com.        NS    ns1.example.com.
node-a.example.com. A     192.0.2.10
node-b.example.com. A     192.0.2.11
```

## Interfaces

Listeners:

- `<wg-ip>:53` — authoritative UDP + TCP for mesh zones.
- `127.0.0.1:53` — loopback bind for the node's own resolver.

Bus verbs (service `dnsd`):

| Verb | Status | Purpose |
|---|---|---|
| `dnsd.zone.snapshot` | v0 | serial-excluded assembled zone snapshot |
| `dnsd.stats` | v0 | per-rcode response counters (e.g. `REFUSED`) |
| `dnsd.zone.{set,adopt}`, `dnsd.reload` | rejected in v0 | future mutation surface |

## Where it fits

Built on the `cosmix-lib-dns` codec library (record model, wire encoding,
strict-data zone parsing, resolver). Depends on `cosmix-lib-config` for node
identity and `cosmix-lib-daemon` for the daemon framework, and registers on the
local [cosmix-noded](noded.md) broker for its Bus surface. Consumes the mesh
inventory that the mesh tooling maintains; [cosmix-webd](webd.md) relies on it
to resolve the vhost names it serves.

## See also

- [noded](noded.md) — the Bus broker dnsd registers with
- [libraries](libraries.md) — `cosmix-lib-dns`, the authoritative DNS codec
- [webd](webd.md) — the web front door whose names dnsd resolves
- [overview](overview.md) — the daemon family at a glance
