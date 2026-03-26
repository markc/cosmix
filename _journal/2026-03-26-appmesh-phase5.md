# 2026-03-26 — Appmesh Phase 5: Infrastructure Apps

## Summary

Completed the final phase of the appmesh ecosystem roadmap. All 5 phases are now implemented. The entire workspace is 707K / 14.7K lines of Rust across 16 crates.

## What was done

### Phase 5 — Infrastructure Apps

Created three new hub-native infrastructure services:

- **cosmix-dns** — Authoritative DNS zone management UI using Hickory DNS (hickory-server/hickory-proto from crates.io). Parses and displays RFC 1035 zone files, record viewer with color-coded types, raw zone editor with save+revalidation, server process status monitoring.

- **cosmix-wg** — WireGuard mesh admin. Parses `wg show all dump` output, displays interfaces with peer tables showing handshake freshness (green/yellow/red), endpoint, allowed IPs, transfer stats. Auto-refreshes every 10 seconds. Hub service port: `wg.*`

- **cosmix-backup** — Proxmox Backup Server dashboard. Talks to PBS REST API with token auth and self-signed cert support. Datastore usage bars, drill-down into snapshots, task list with status coloring. Hub service port: `backup.*`

### cosmix-dns evolution

Started as a PowerDNS API client (wrong — no PDNS on the network). Corrected to use the Hickory DNS fork from cosmix-cosmic. Went through three iterations:

1. Standalone workspace with vendored Hickory DNS source (2.5M, 130K+ lines)
2. Merged into cosmixos workspace — resolved all workspace refs, fixed path deps, upgraded rusqlite 0.32→0.38 across cosmixos+spamlite to resolve sqlite linking conflict
3. Replaced vendored source with crates.io deps (hickory-server/hickory-proto 0.25), flattened to standard crate structure — 29K → 16K

### Workspace cleanup

- Removed all Hickory DNS cruft: tests, conformance, fuzz, audit, scripts, docs, logos, licenses
- Fixed cosmix-embed for rusqlite 0.38 API change (usize → i64 cast in query_row)
- Removed redundant .gitignore from cosmix-dns
- Final state: 16 crates, 707K total, no dead files

## Commits

```
3c8d85d Remove redundant .gitignore from cosmix-dns
e20c30d Replace vendored Hickory DNS with crates.io deps, flatten cosmix-dns
caa7d35 Merge cosmix-dns into cosmixos workspace, remove cruft
2932222 Implement appmesh Phase 5: infrastructure apps (cosmix-dns, cosmix-wg, cosmix-backup)
```

## Roadmap status

All 5 appmesh phases complete:

| Phase | What | Crates |
|-------|------|--------|
| 1 | Foundation | cosmix-ui, cosmix-hub, cosmix-client |
| 2 | Inter-App | cosmix-files, cosmix-view retrofit |
| 3 | Mesh | hub mesh bridge, cosmix-mon, cosmix-mesh |
| 4 | Productivity | cosmix-edit, cosmix-mail retrofit |
| 5 | Infrastructure | cosmix-dns, cosmix-wg, cosmix-backup |

## Workspace at a glance

```
16 crates, 707K on disk, 14.7K lines of Rust
├── Libraries: cosmix-port, cosmix-mesh, cosmix-client, cosmix-ui
├── Hub: cosmix-hub (WebSocket broker + mesh bridge)
├── Desktop apps: cosmix-mail, cosmix-view, cosmix-files, cosmix-edit, cosmix-mon
├── Infrastructure: cosmix-dns, cosmix-wg, cosmix-backup
├── Servers: cosmix-jmap (JMAP/SMTP), cosmix-embed, cosmix-web
```
