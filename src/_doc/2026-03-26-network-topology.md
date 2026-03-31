# Network Topology — Gold Coast Infrastructure

> Snapshot: 2026-03-26

## Physical Layout

```text
Internet (NBN 1000/100)
    │
    ▼
┌─ gw (GL-iNet GL-MT6000, OpenWrt) ─────────────────────────
│  WAN: 120.88.117.136/22 (eth1, 2.5G)
│  LAN: 192.168.1.1/24 (br-lan)
│  WG:  172.16.1.1/24 (wg0:51820), 172.16.2.1/24 (wg1:51822)
│  Services: nginx, postfix, dovecot, dnsmasq, opendkim, crowdsec
│  SSL: *.kanary.org wildcard cert
└───────────────────────────────────────────────────────────
    │ LAN (192.168.1.0/24, untrusted)
    │
    ├── gw2 (OpenWrt CT on pve5) ◄── DOUBLE-NAT BOUNDARY
    │   WAN: 192.168.1.200 (eth1)
    │   LAN: 192.168.2.1/24 (br-lan)
    │   Masquerade: ON, inbound blocked
    │   DHCP: .100–.249, dnsmasq DNS
    │   │
    │   │ Inner LAN (192.168.2.0/24, secured)
    │   │
    │   ├── cachyos    192.168.2.10   workstation (CachyOS minipc)
    │   ├── pve5       192.168.2.25   Proxmox host
    │   ├── pve2–4     192.168.2.22–24  Proxmox nodes
    │   ├── pbs2–4     192.168.2.32–34  Backup servers
    │   ├── mko        192.168.2.210  mail server (cosmix)
    │   ├── jellyfin   192.168.2.120  media (dual-homed)
    │   └── ollama     192.168.2.130  LLM inference
    │
    ├── pve5 vmbr4     192.168.1.250  (outer LAN bridge)
    ├── mko eth1       192.168.1.210  (outer LAN, direct)
    └── jellyfin eth1  192.168.1.120  (outer LAN, DLNA)

Corosync (air-gapped): 192.168.10.0/24 (vmbr1, PVE cluster only)
```

## WireGuard Mesh

```text
wg0 (172.16.1.0/24, port 51820) — infrastructure mesh
    Hub: gw (172.16.1.1)
    ├── cachyos   172.16.1.4   (via 192.168.1.200)
    ├── pixel     172.16.1.2   (mobile)
    ├── s76       172.16.1.3
    ├── pve1-ch   172.16.1.5   (Sydney 206.83.113.50, routes 10.10.10.0/24)
    ├── pve2-ch   172.16.1.6   (Sydney, backup route)
    ├── cherry    172.16.1.7   (144.6.248.36)
    ├── haproxy   172.16.1.8
    ├── mmc       172.16.1.9   (203.25.132.25)
    └── mko       172.16.1.210 (192.168.1.210)

wg1 (172.16.2.0/24, port 51822) — mail/services mesh
    Hub: gw (172.16.2.1)
    ├── cachyos   172.16.2.5
    ├── gc        172.16.2.4
    ├── mmc       172.16.2.9
    ├── mko       172.16.2.210  ◄── cosmix-jmap listens here
    └── test peers 172.16.2.12–21 (mostly stale)

wgtest (10.200.0.0/24, port 51821) — nameserver mesh
    Hub: gw (10.200.0.1)
    ├── ns1gc     10.200.0.10  (Sydney)
    ├── ns2gc     10.200.0.20  (Brisbane)
    └── ns3gc     10.200.0.30  (Melbourne)
```

## DNS Resolution

```text
cachyos query → systemd-resolved (127.0.0.53)
  → gw2 dnsmasq (192.168.2.1)
    → gw dnsmasq (192.168.1.1) if not local
      → 9.9.9.9 (Quad9) for external

Local DNS entries (gw dnsmasq):
  kanary.org       → 192.168.2.210 (mko)
  mail.kanary.org  → 192.168.2.210 (mko)
  *.goldcoast.org  → various inner LAN IPs

mko DNS (systemd-resolved → 127.0.0.53):
  No upstream configured! ◄── THIS IS THE OUTBOUND SMTP DNS FAILURE
```

## Key Issue: mko DNS

mko's resolver points to `127.0.0.53` (systemd-resolved stub) but has no
upstream DNS server configured. It cannot resolve external domains, which
is why outbound SMTP delivery fails with "Name or service not known".

Fix: configure mko's systemd-resolved to use gw2 (192.168.2.1) as upstream.

## Listening Services on mko

| Port | Service | Bind address |
|------|---------|-------------|
| 25   | SMTP inbound | 0.0.0.0 |
| 443  | cosmix-web (HTTPS) | 0.0.0.0 |
| 465  | SMTPS | 172.16.2.210 |
| 8443 | JMAP API (HTTPS) | 172.16.2.210 |
| 51820| WireGuard | 0.0.0.0 |
