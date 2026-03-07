# ARexx Lineage: From Amiga to Cosmix

Date: 2026-03-06

## What Made ARexx Special

ARexx (1987) wasn't just a scripting language. It was a **universal IPC system** built into the operating system. Every Amiga application could expose an ARexx port, and any ARexx script could talk to any combination of apps.

### The Pattern

```
┌──────────┐  ARexx Port  ┌──────────┐
│  App A   │◄────────────►│          │
└──────────┘              │  ARexx   │
┌──────────┐  ARexx Port  │  Script  │
│  App B   │◄────────────►│          │
└──────────┘              │          │
┌──────────┐  ARexx Port  │          │
│  App C   │◄────────────►│          │
└──────────┘              └──────────┘
```

A single script could:
1. Open a document in a text editor
2. Search/replace text
3. Pass the result to a graphics program
4. Render it
5. Send it via a comms program

No APIs, no SDKs, no build tools. Just a script.

### Why Nothing Replaced It

Every subsequent desktop environment (Windows, macOS, GNOME, KDE) had scripting, but none achieved ARexx's universality:

| System | Scripting | Why it fell short |
|--------|-----------|-------------------|
| Windows COM/OLE | VBScript, JScript | Complex, app support inconsistent |
| macOS AppleScript | AppleScript | Verbose, slow, apps don't invest in it |
| GNOME | Python + D-Bus | No standard port pattern, each app is different |
| KDE | D-Bus + KParts | Better than GNOME but still ad-hoc, QML fragmentation |

The missing ingredient was always **convention**: ARexx worked because Commodore made it part of the OS and apps were expected to support it.

## Why COSMIC + Cosmix Can Succeed

### COSMIC provides the convention

- Every COSMIC app is Rust + iced
- Every COSMIC app will have D-Bus interfaces (cosmic-comp already does)
- System76 controls the full stack — they can establish conventions
- Unlike KDE/GNOME, there's no legacy baggage or competing toolkits

### Cosmix provides the scripting layer

- Lua embedded in Rust via mlua
- AppMeshPort trait = standardised port interface (like ARexx ports)
- AMP protocol = human-readable IPC (like ARexx message passing)
- meshd extends it across the network (ARexx was local-only)

### The evolution

```
ARexx (1987)          → Cosmix (2026)
─────────────────────────────────────────
REXX language         → Lua (via mlua)
ARexx ports           → AppMeshPort trait
Amiga message ports   → D-Bus + Unix sockets
Local machine only    → Mesh network (meshd + WireGuard)
Text-based IPC        → AMP (markdown frontmatter)
CLI: rx script.rexx   → cosmix run script.lua
AmigaOS               → COSMIC desktop
68000 native apps     → Rust native apps
```

## The Mesh Extension

ARexx's limitation was the local machine. Cosmix extends the model:

```lua
-- Local: automate COSMIC apps on this machine
local files = mesh.port("cosmic-files")
files:open("/home/cosmic/project")

-- Remote: trigger deployment on production server
local mko = mesh.node("mko")
mko:send("deploy", { repo = "markweb", branch = "main" })

-- Cross-node: get status from all mesh nodes
for _, node in ipairs(mesh.nodes()) do
    local status = node:query("status")
    print(node.name .. ": " .. status.uptime)
end
```

Same scripting model, same port pattern, extended across WireGuard mesh.

## Historical Context

Mark Constable (born 1954) experienced the Amiga era firsthand. The ARexx comparison isn't nostalgia — it's pattern recognition. The conditions that made ARexx work (single-language native apps, OS-level convention, simple scripting) are re-emerging with COSMIC for the first time in nearly 40 years.

The difference: Cosmix has networking (meshd), AI integration (markweb agents), and modern tooling (Rust safety, Lua performance). ARexx was limited by 1987 hardware and local-only IPC. Cosmix has none of those constraints.
