# cos — component docs

Summary documentation for the **cos** daemon family, one page per component. The
canonical home of these pages is
[`docs/_man/`](https://github.com/markc/cosmix/tree/main/docs/cos) in the public
[markc/cos](https://github.com/markc/cosmix) repo; add a page by dropping
`COMPONENT.md` there. The same files render three ways:

- **Web** — [markc.github.io/cos](https://markc.github.io/cos/) serves this manual in the site's left-hand **Components** pane; any page deep-links as `#_man/PAGE.md`.
- **GitHub** — browse [`docs/_man/`](https://github.com/markc/cosmix/tree/main/docs/cos) directly; this file doubles as the directory README.
- **Local clone** — plain markdown with relative links; any editor or viewer works.

These are **summaries** — what each component is and how to use it — not
exhaustive manpages. For the language that drives these daemons, see the
[mix manual](https://markc.github.io/mix/).

## Start here

- **[overview](overview.md)** — what cos is, how the workspace is laid out, how it fits with bus and mix.

## Daemons

- **[noded](noded.md)** — the Bus broker; every node runs one.
- **[maild](maild.md)** — JMAP-native mail daemon (SMTP + IMAP + JMAP + spam).
- **[webd](webd.md)** — multi-vhost HTTPS + ACME; server-rendered web UI.
- **[dnsd](dnsd.md)** — authoritative WireGuard-mesh DNS.
- **[indexd](indexd.md)** — vector knowledge base / indexer.
- **[desktop](desktop.md)** — the CosMix desktop: `cosmix-comp` (Smithay + Bevy Wayland compositor), the `ctk` toolkit and the native apps.
- **[cosmix-comp](cosmix-comp.md)** — Wayland compositor and supported protocol globals.
- **[quoin](quoin.md)** — four-edge layer-shell desktop furniture hosted through SCTK.
- **[agentd](agentd.md)** — agent supervision.
- **[powerd](powerd.md)** — event-driven UPower battery and power state.
- **[mprisd](mprisd.md)** — event-driven MPRIS2 media-player state and delegated controls.

## Bridge & libraries

- **[foreman](foreman.md)** — task runner, policy gate, verifier, and refinery operator runbook.
- **[mcp](mcp.md)** — the Claude Code MCP bridge.
- **[libraries](libraries.md)** — the shared `cosmix-lib-*` substrate crates.

## See also

- Repos: [markc/cos](https://github.com/markc/cosmix) · [markc/mix](https://github.com/markc/cosmix) · [markc/bus](https://github.com/markc/cosmix).
- The [mix manual](https://markc.github.io/mix/) — the language and shell used to operate these daemons.
