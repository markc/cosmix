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

## Shared fonts, icons and emoji

Shared asset publication is opt-in. Set `webd.shared_assets_dir` in
`node.conf.mix`, or pass `serve --assets-dir /srv/cosmix-assets`. The value
is an absolute installation root containing `sets/<set-id>/`; it is never
discovered from the server account's XDG user data. Installers normally use
`cosmix_path("share")/assets`, with user overrides kept separate.

Known vhosts then serve the same installed bytes at
`/_cos/assets/<set-id>/<manifest-path>`. For example, a page can load
`/_cos/assets/2026-10-01-core-2/fonts.css`; its relative font URLs stay bound
to that retained set. The existing per-vhost `/assets/` route keeps its own
meaning. There is no `current` URL or directory listing, and only files in
the manifest plus `fonts.css` and `manifest.conf.mix` are public.

At startup, webd verifies both locked hashes for each published set, up to
32 retained sets. New installations become available after a webd restart.
Modified or symlinked files are refused until the installation is corrected
and reverified. Versioned responses have correct font/CSS MIME types,
`nosniff`, SHA-256 ETags and a one-year immutable cache policy; failed
responses use `no-store`. HEAD, byte ranges and Last-Modified revalidation
are supported. Fonts are served on the page's own origin; cross-origin
access is not enabled by default.

For pages on another node or origin, explicitly enable
`webd.shared_assets_cross_origin: true` or `serve --assets-cross-origin` alongside
the asset directory. Shared asset responses then allow anonymous cross-origin
reads with `Access-Control-Allow-Origin: *`; credentials are never enabled.

For an isolated loopback preview, combine the two explicit roots:

```sh
cosmix-webd serve --static-dir /srv/preview --assets-dir /srv/cosmix-assets
```

webd supplies read-only HTTP access to these local files. Each mesh node
still installs its own local set for native applications and offline use;
the directory path itself does not become a shared mesh filesystem.

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
