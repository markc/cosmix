# cosmix-webd — multi-vhost HTTPS front door

**`cosmix-webd` is the mesh's web server: multi-vhost HTTPS with automatic
ACME certificates, and a server-side-rendered UI that streams updates over
Datastar SSE.** One binary terminates TLS for many domains and serves the
mail, files, and CMS surfaces of the substrate as ordinary web pages.

## What it is

A long-running Rust daemon that owns a node's `:443` (and optional `:80`)
listeners. It is the human-facing edge of the mesh — where a browser meets the
substrate. Vhosts, routes, TLS certificates, and page templates are all
configured through Bus verbs and the SPEC-12 property store, not hand-edited
config files, so an agent can add a site or rotate a certificate with a
structured call.

Pages are **server-side rendered**. Interactive behaviour comes from
[Datastar](https://data-star.dev) — the server pushes DOM patches over a
Server-Sent-Events stream (`text/event-stream`) instead of shipping a
client-side framework. Handler logic for a vhost is written in
[Mix](https://github.com/markc/cosmix) and evaluated per request, with seams into
the mail daemon (`jmap()`), the files daemon, and the CMS database.

## What it does

- **TLS termination + SNI** for many vhosts on one listener, backed by a per-vhost certificate resolver.
- **Automatic ACME** (Let's Encrypt style) — provisions and renews certificates over the HTTP-01 challenge on the `:80` listener, with cooldown and renewal-window gates.
- **SSR web UI** — renders mail (JMAP-backed), a dual-pane file manager, and a CMS/PIM, patching the page live over Datastar SSE.
- **Per-vhost Mix handlers** — request routing and page logic authored in Mix, with `$SIGNALS` (the parsed Datastar signal store), `$BODY`, and session identity injected into scope.
- **Sessions + auth** — cookie sessions (`cosmix_session`), server-side authentication for every write; a loopback-gated dev auto-session for headless local preview.

## Running it

```sh
/opt/cosmix/bin/cosmix-webd
```

Runs under systemd as `cosmix-webd.service` (identity `User=cosmix-webd`, from
the SPEC-10 sysusers fragment). Shared node settings load from
`/etc/cosmix/node.toml`; the per-daemon block, listener addresses, and vhost
tree come from `/etc/cosmix/webd/config.toml` plus the property store. A
loopback-only dev listener (default `127.0.0.1:8080`) offers a zero-config
local preview fenced to localhost.

## Interfaces

Listeners:

- `:443` — TLS vhosts (public edge).
- `:80` — plain HTTP: ACME HTTP-01 challenge + optional redirect / autoconfig.
- `127.0.0.1:8080` — internal dev/preview listener (opt-in).

Bus verbs (service `webd`):

| Verb | Purpose |
|---|---|
| `webd.vhost.add` | register a new vhost |
| `webd.props.{list,get,set,delete}` | property store surface |
| `webd.routes.list` | list matched routes |
| `webd.acme.{status,renew}` | certificate state; force a renewal |
| `webd.tls.{status,reload}` | TLS resolver state; hot-reload certs |
| `webd.autoconfig.served_domains` | mail-client autoconfig domains |
| `webd.session.revoke` | revoke a session |
| `webd.stats` | request/response counters |

## Where it fits

Depends on `cosmix-lib-daemon` (the `tls` feature: rustls + ACME + SNI),
`cosmix-lib-props-store` (SPEC-12 state), `cosmix-lib-config`, and the Bus
client libraries from [bus](https://github.com/markc/cosmix). It reaches a local
or remote `cosmix-maild` over JMAP for mail, the files daemon for the file
manager, and evaluates per-vhost handlers through the embedded
[mix](https://github.com/markc/cosmix) engine. Every node needs a
[cosmix-noded](noded.md) broker for the Bus surface.

## See also

- [noded](noded.md) — the Bus broker webd registers with
- [maild](maild.md) — the JMAP mail backend behind the web mail UI
- [dnsd](dnsd.md) — mesh DNS that resolves the vhost names webd serves
- [libraries](libraries.md) — `cosmix-lib-daemon`, `cosmix-lib-props-store`
- [overview](overview.md) — the daemon family at a glance
