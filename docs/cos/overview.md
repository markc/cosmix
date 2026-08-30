# cos — overview

**cos** is the cosmix daemon family: every Rust crate that runs a node in a
cosmix mesh, plus the substrate libraries they share. Where
[mix](https://github.com/markc/mix) is the language you drive the mesh *with*,
cos is what actually *runs* on each node — a set of long-running daemons that
speak the Bus mesh protocol, expose their state as typed property namespaces,
and are operable from the outside by agents and scripts.

This page is the orientation: what cos is, how the workspace is laid out, how it
fits with its sibling repos, and where to go next.

## What cos is

A single Cargo workspace of ~33 crates, split three ways:

- **Daemons** — the long-running services: an Bus broker, a mail server, a web server, an authoritative DNS server, a knowledge indexer, a display compositor, and an agent supervisor.
- **Helpers & adapters** — CLI subcrates and integration shims the daemons lean on (the MCP bridge, the Claude SDK adapter, the mail data store, the mail auth / rules / spam subcrates).
- **Libraries** — the shared substrate (`cosmix-lib-*`): property store, mesh peering, node identity, WireGuard config, logging, agent runtime pieces, and protocol codecs.

Every daemon is built to be **legible** (query its state as structured data),
**modifiable** (write its config through structured channels, not ad-hoc file
edits), and **reconstructible** (build and run it from a clean clone). Those
three criteria are the substrate's design filter — the point of cos is a system
an AI agent can observe, change, and rebuild.

## The components

Pick any component in the sidebar for its own page.

### Daemons

- **[noded](noded.md)** — the Bus broker. Every mesh node runs one; it routes `service.node.bus` addresses across the mesh.
- **[maild](maild.md)** — JMAP-native mail daemon (SMTP + IMAP + JMAP, Bayesian spam classifier, Sieve-style rules).
- **[webd](webd.md)** — multi-vhost HTTPS with automatic ACME certificates; server-rendered web UI over Datastar SSE.
- **[dnsd](dnsd.md)** — authoritative WireGuard-mesh DNS, zones generated from the mesh inventory.
- **[indexd](indexd.md)** — vector knowledge base; auto-indexes workspaces on commit and powers agent recall.
- **[disp-skia](disp-skia.md)** — Skia display compositor rendering a markdown-over-Bus UI: the desktop surface.
- **[agentd](agentd.md)** — agent supervision.
- **[powerd](powerd.md)** — event-driven UPower battery and power-source state.

### Bridge & libraries

- **[mcp](mcp.md)** — the Claude Code MCP bridge; exposes cosmix surfaces as MCP tools plus a knowledge / skill-learning loop.
- **[libraries](libraries.md)** — the shared `cosmix-lib-*` substrate crates.

## How it fits — bus ← mix ← cos

cos is one of three public sibling repos with a one-way dependency order:

- **[bus](https://github.com/markc/bus)** — the Bus protocol family. Depends on nothing.
- **[mix](https://github.com/markc/mix)** — the language. Depends on bus.
- **cos** — the daemon family. Depends on both: its config, DNS, mail, and agent crates consume the mix strict-data parser, and every daemon speaks Bus.

Neither sibling depends on cos. Build bus and mix first (see
[Build & install](#build)); the workspace clones expect sibling checkouts at
`$COSMIX` and `$COSMIX`.

## Building

```sh
# cos depends on two sibling checkouts under $HOME
git clone https://github.com/markc/bus $COSMIX
git clone https://github.com/markc/mix $COSMIX
git clone https://github.com/markc/cos $COSMIX

# build the whole daemon family (release)
cd $COSMIX/src && cargo build --workspace --release
```

Each daemon reads a small root-owned bootstrap config
(`/etc/cosmix/<daemon>/config.toml`) before it contacts the mesh, then joins the
Bus broker and exposes its runtime state through property namespaces.

## See also

- **[the component index](README.md)** — every page in this manual.
- Repos: [markc/cos](https://github.com/markc/cos) · [markc/mix](https://github.com/markc/mix) · [markc/bus](https://github.com/markc/bus).
