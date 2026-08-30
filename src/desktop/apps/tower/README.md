# CosMix Tower

CosMix Tower is the native CTK/Bevy mission-control surface for the CosMix
mesh. It presents the verified noded inventory, the local broker's configured
peer edges, connected citizens and the property summaries that existing
daemons expose today. It also provides same-node citizen controls and a live,
redaction-aware broker traffic view.

Its stable component slug is `tower`; the binary is `cosmix-tower`, the native
application id is `dev.cosmix.tower`, and runtime state lives below
`cosmix/apps/tower`. See [the desktop registry](../../APPS.md).

```sh
cd ~/.cos/desktop
cargo run -p cosmix-tower
```

Use `--noded-url <url>` to connect to a non-default local broker endpoint.

## P1 data and freshness

Tower opens one CTK Bus bridge to local noded. The bridge owns independent
control and subscription sockets; P3 adds a third, opt-in observation socket
so live traffic cannot block atlas or operator RPCs. Tower reads:

- `noded.inventory` for the broker-verified authority projection;
- `noded.peers` for local configured edges;
- `<node>.bus` `noded.info` and `noded.list` for active Bus members;
- flat local `<service>.props.{list,get,describe}` surfaces.

`maild` and `webd` are shown as `namespace required`; Tower does not guess
their namespaces.

The atlas bootstraps at startup and after an Bus reconnect. Local retained
`world.noded`, `world.indexd` and `world.musicd` topics update the matching
surfaces; `interact.props.changed` causes one `interact.props.get`. There is no
recurring data poll. Remote data refreshes only on reconnect or **Refresh
mesh**, retains its `observed_at` value, and becomes `unknown` rather than
`down` when a later read fails.

The topology draws one entity per signed-inventory member. Only edges returned
by the local `noded.peers` call are drawn; Tower never infers a complete mesh.
Live observation events pulse those declared local edges when their endpoints
map through fresh peer and citizen discovery. Stale caches, ambiguous events
and events for undeclared pairs pulse a separate marker on the local node, so
traffic is not silently lost and no route is invented. Pulse decay runs only
while activity remains hot.

## P2 citizen and daemon controls

Selecting a citizen on the local node reads its exact process-scoped service
name with `app.describe`, `actions.list`, `actions.describe`,
`app.controls.list` and `app.controls.get`. Nullary Bus-enabled actions and
writable CTK controls are available through explicit CTK confirmation dialogs.
Control values are event-driven: Tower re-reads one control after setting it,
or all selected-citizen controls during an explicit mesh refresh. It never
polls them. A confirmed mutation captures the citizen's process identity and
is revalidated immediately before Bus dispatch; the same action or control is
disabled while its mutation is queued or in flight. Identity is checked again
when the reply arrives, and stale replies cannot update the inspector or
trigger a control re-read. A citizen without either `pid` or `started_at` is
treated as identity-unknown. Inspector payloads and rendered entity counts are
bounded.

The node Overview pane discovers `cosmix-*.service` units through SSH and
offers Start, Stop and Restart. Stop and Restart require confirmation. Commands
use `BatchMode=yes`, `ConnectTimeout=5` and a 15-second outer timeout; unit
names are validated before process spawn. Every queued command is tied to its
verified inventory epoch and expires after 30 seconds before execution. Unit
discovery passes no wildcard through the remote login shell: Tower lists
services and filters strict `cosmix-*.service` names locally.

Tower reads optional SSH aliases from `nodes.conf.mix` in its app config
directory. Missing entries default to the inventory node name:

```mix
ssh_aliases: {
  alpha: "alpha-admin",
  storage: "storage-ops"
}
```

Cross-node app mutation, remote quit, arbitrary verb execution, Bus daemon
lifecycle and bulk controls remain out of scope.

## P3 live traffic

The Traffic detail pane owns a SPEC 02 §4.2 observation subscription. Opening
the pane starts it; closing Tower or moving to another detail pane stops it.
Reconnect creates a fresh subscription, with no replay or resume. The stream
is the only traffic data source: Tower never polls for observation data.

The pane offers anchored verb-glob, exact-service and direction filters.
Metadata-only observation is the default. The **Bodies** control explicitly
requests broker-redacted structured payloads; opaque, oversized or
policy-refused payloads remain omitted. Both the live scrollback and the
client-side pause buffer are bounded to 2048 events, and broker-side plus
client-side drops are shown separately.

Observation is fail-closed until the local noded operator grants Tower's
process-scoped service identity. Add this top-level block to each node's
`/etc/cosmix/node.conf.mix`, merge it with any existing patterns, then restart
`cosmix-noded`:

```mix
observe: {
  allowed_services: [
    "tower-bevy-*"
    "log-observe-*"
  ]
}
```

`log-observe-*` is required only when noded's built-in traffic logger is
enabled. The repository example is
`src/crates/cosmix-lib-config/examples/node.conf.mix`; this change does not
edit deployed `/etc` configuration.

## P4 saved workspace state

Tower atomically stores local UI state in
`cosmix/apps/tower/config/state.conf.mix` through CTK's app-directory surface.
The schema-versioned file contains named traffic filters, the current and
last-active filter, canvas pan/zoom, and each DCS sidebar's visibility,
pinning, width and active panel. Unsupported or malformed schemas are loaded
fail-closed and are not overwritten. `schema_version` is required; catalogues
that need truncation or name sanitisation also load read-only. Failed writes
remain pending for retry, and app exit gives the latest state a bounded
best-effort flush.

The Filters pane can save the current traffic controls under a name, select a
saved set, or delete one. A filter may name a service that is currently absent;
it remains loaded and simply matches nothing. Persisted state never contains
inventory members or edges.

The current topology canvas does not support manual node dragging, so P4
persists pan and zoom only. Tower has no draggable centre split; its persisted
pane split state is the two DCS sidebar widths plus open/pinned preferences.
