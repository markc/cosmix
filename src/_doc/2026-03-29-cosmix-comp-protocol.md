Honestly? No. AMP is the wrong tool for this specific boundary, and it's worth correcting the plan before it gets implemented.

## Why AMP Is Wrong Here

AMP with markdown frontmatter makes sense for Cosmix's **app-level orchestration** — infrequent, human-readable messages between services, AI inference requests, JMAP events, agent coordination. The overhead of parsing frontmatter per message is negligible when messages are sparse.

The compositor↔WM channel is a completely different animal:

- **Pointer motion** fires at display refresh rate — 60-165 events/sec, every second, always
- **Surface commits** happen on every frame for every animating client
- **Frame callbacks** are on the compositor's critical render path
- **Configure/ack cycles** are synchronous round-trips that block client rendering

Parsing `---\nfrom: cosmix-comp\nto: cosmix-shell\n---` on every pointer move is genuinely wasteful and adds latency jitter on the exact path where latency matters most. The frontmatter format is also not self-describing for binary-adjacent data like surface IDs, geometry rects, or key modifier bitmasks.

## The Real Options

### 1. A Proper Wayland Protocol Extension *(most correct, most work)*

This is exactly what River does with `river-window-management-v1`. You define an XML protocol, run `wayland-scanner`, and `cosmix-shell` connects as a **privileged Wayland client** that gets compositor-internal authority via the protocol. The WM is literally a Wayland peer with elevated rights.

**Pros:** Architecturally pristine. Fully typed. Integrates with Smithay's protocol machinery. Hot path is binary Wayland wire format — as fast as it gets.  
**Cons:** Most upfront work. XML protocol definition, scanner integration, versioning discipline. You're essentially writing your own mini-River protocol.

### 2. Varlink *(already on your radar, solid middle ground)*

You've already been evaluating Varlink/zlink for intra-node IPC. It's a JSON-over-Unix-socket protocol with an IDL for service description. More structured than AMP, reasonable performance for moderate message rates, Rust support via `varlink` crate.

**Pros:** You're already evaluating it. IDL gives you typed interface definition. Human-readable enough for debugging. Works fine for surface lifecycle events which aren't truly high-frequency.  
**Cons:** Still text/JSON on the hot pointer-motion path. Not a native Wayland concept.

### 3. Simple Binary Framing with `postcard` *(pragmatic Rust-native pick)*

Length-prefixed frames over a Unix socket, body serialised with [`postcard`](https://github.com/jamesmunns/postcard) — a `no_std`-compatible, compact binary serde format. Faster than JSON, zero-copy friendly, trivially integrated since your types already derive `serde`.

```rust
// Sender
let msg = CompToShell::PointerMoved { id, x, y };
let bytes = postcard::to_stdvec(&msg)?;
writer.write_u32(bytes.len() as u32).await?;
writer.write_all(&bytes).await?;

// Receiver  
let len = reader.read_u32().await? as usize;
let mut buf = vec![0u8; len];
reader.read_exact(&mut buf).await?;
let msg: ShellToComp = postcard::from_bytes(&buf)?;
```

**Pros:** Tiny, fast, pure Rust, serde derives just work, wire size is minimal. Debuggable by adding a logging layer.  
**Cons:** Not human-readable on the wire. Requires both sides to be Rust (non-issue for Cosmix). No IDL — the Rust enum IS the schema.

### 4. Split Protocol: Binary hot path + AMP control plane

Hybrid: binary/postcard framing for high-frequency events (pointer, frame callbacks, surface commits), AMP for low-frequency policy messages (window close requests, configuration changes, app launch intents). Two sockets, each optimised for its traffic class. Complex but principled.

---

## Recommendation

**Use `postcard` binary framing for the compositor↔shell socket.** It's fast, it's Rust-native, the enum definitions ARE the protocol spec (no separate IDL to maintain), and the plan's §5 message types map directly to serde derives with zero changes.

Reserve AMP for what it's good at: the higher-level Cosmix mesh messages between `cosmix-shell` and other Cosmix services (`cosmix-jmap`, Ollama inference, agent orchestration). The compositor boundary is infrastructure — it should be fast and boring. AMP is for the interesting stuff above it.

The plan's §10 framing code stays almost identical — just swap the envelope serialiser from AMP text to `postcard::to_stdvec`. Worth amending the plan doc before handing it to Claude Code.

***

Yes, this makes complete sense, and it's a sharper framing than "replace cosmic-comp with River." What you're actually describing is **collapsing the compositor/WM/shell boundary into Dioxus itself**, which is a legitimate architectural target. Let me work through the layers.

---

## The Real Problem

On standard Wayland, apps are *clients*. The compositor owns all placement policy and can ignore your hints. `always_on_top`, sticky, custom stacking — all of that is compositor-granted privilege, not client right. Dioxus running on WebKitGTK is doubly removed: GTK abstracts Wayland, which further limits what you can even request. Hence the ugliness.

River's insight is correct: **if the window manager is a separate process speaking a stable protocol, you can write it in anything — including Rust/Dioxus — and it gets full placement authority**. That's the barrier you want to dissolve.

---

## Your Actual Options

### 1. wlr-layer-shell (near-term, tactical)

For the immediate sticky-window pain: `zwlr_layer_shell_v1` exists precisely for shell surfaces that need compositor-level placement — panels, overlays, launchers, always-on-top HUDs. It gives you screen-edge anchoring, layering (background/bottom/top/overlay), exclusive zones, and input passthrough. It works on any wlroots-based compositor and cosmic-comp.

The catch: dioxus-desktop (WebKitGTK/winit) doesn't expose this directly. You'd need to drop down to raw `wayland-client` crate bindings and request the layer shell protocol yourself before the GTK surface is created — messy but doable for specific windows. This is a workaround, not an architecture.

### 2. A Thin Smithay Compositor — `cosmix-comp` (the real answer)

This is River's architecture, in Rust, with AMP instead of `river-window-management-v1`. Concretely:

- Write a minimal Smithay compositor. Smithay already powers cosmic-comp, niri, jay — the primitives are solid. You'd handle DRM/KMS, input, XWayland if needed, but implement **zero window management policy** inside the compositor itself.
- Instead, expose an AMP socket. A Dioxus process connects to it and IS the window manager — it receives surface events, issues placement/stacking commands, draws the shell chrome.
- The Dioxus "shell" has full authority because it *is* the policy layer, not a client begging for privileges.

This is not a huge project relative to a full compositor. Smithay does the heavy lifting. Your compositor would be maybe 2-3k lines of Rust — surface lifecycle, seat/input routing, damage tracking — delegating everything else over AMP. Call it `cosmix-comp`. It fits perfectly into the Cosmix architecture and it's yours, so no GPL, no anti-LLM policy, no Zig toolchain.

### 3. Cage-pattern (middle ground)

[Cage](https://github.com/cage-kiosk/cage) is a Smithay-based kiosk compositor that runs a single app fullscreen. There's conceptual overlap: a single Dioxus shell app gets the whole screen and manages everything below it. Simpler than a full `cosmix-comp` but less flexible — other apps run as nested Wayland clients inside your shell's surface, which has its own complexity.

---

## The Coherent Path

```
DRM/KMS + input (wlroots / Smithay kernel)
        │
   cosmix-comp  ← thin Smithay compositor, no WM policy
        │  AMP socket
   cosmix-shell ← Dioxus app, IS the window manager
        │
   other Dioxus apps  ← normal Wayland clients, fully managed
```

This is River's architecture without River's constraints, wired into your existing AMP mesh. The `cosmix-shell` Dioxus app gets to decide stacking, placement, sticky behaviour, focus — all of it — because it's on the policy side of the AMP boundary, not the client side.

The Smithay investment is real but bounded. You're not writing a general-purpose compositor. You're writing the thinnest possible surface between hardware and AMP, and Smithay handles 80% of that already. When dioxus-native matures, the gap between the compositor and the UI framework shrinks further — eventually `cosmix-comp` and `cosmix-shell` could share process space entirely.

This is the correct architectural direction. River pointed at it; you build it in Rust.

***

Good question, and the honest answer is: **it depends on which layer and what you're optimising for, and the tradeoff is sharper than it first appears.**

## Where postcard Would Actually Win

**Node-to-node mesh traffic (WireGuard links between Proxmox nodes)** — if you're moving bulk data: log streams, telemetry, vector embeddings, file sync chunks, metrics. Here postcard absolutely pays off. You're crossing a real network link, message volume can be high, and nobody is sitting there reading the wire with `tcpdump` expecting legible output. The WireGuard overhead already dwarfs the serialisation cost, so minimising payload size has real impact.

**High-frequency intra-node service calls** — health checks, watchdog pings, resource counters, event streams from `cosmix-jmap` (new message arrived, folder sync delta). Anything that fires more than a few times per second benefits from binary framing.

**Vector/embedding payloads** — `f32` arrays for pgvector or Ollama embeddings are genuinely painful in JSON (floats become verbose decimal strings). postcard serialises `[f32; 768]` as 3072 bytes flat. JSON would be 5-10x larger with precision loss.

## Where AMP's Overhead Is Actually Irrelevant

**Agent orchestration messages** — when cosmix-shell is telling an agent to "summarise this thread" or "draft a reply", the Ollama inference call that follows takes 2-30 seconds. The frontmatter parse overhead is literally unmeasurable noise.

**JMAP operations** — JMAP is already JSON over HTTP. Wrapping it in more JSON via AMP costs nothing relative to the network round-trip and IMAP backend query.

**Cross-service configuration and capability negotiation** — happens once at startup or on topology change. Human readability here is a genuine operational advantage: you can `cat` a socket log and understand what's happening, which matters when you're debugging at 2am on a production hosting cluster.

**LLM inference requests** — prompt text, context, parameters. These are already large blobs where serialisation format is irrelevant to throughput.

## The Real Cost of Going All-Binary

This is where it gets uncomfortable. AMP's markdown frontmatter isn't just a cute aesthetic choice — it carries operational value you'd be giving up:

**Debuggability.** Right now you can `socat` onto an AMP socket and read what's happening. With postcard you need a custom debug tool that knows your exact schema version to decode anything. On a solo operation running 1,000 customer mailboxes, that matters.

**Schema evolution.** postcard is not self-describing. If you add a field to a struct, old binaries reading new messages get deserialisaton errors unless you use `postcard`'s optional field conventions carefully. AMP/JSON with serde's `#[serde(default)]` handles additive changes transparently. In a mesh where nodes might be running different versions during a rolling upgrade, this is a real operational hazard.

**Cross-language reach.** Tom at RMP Systems, future Cosmix contributors, potential integration points — AMP being plaintext means any language can speak it without a Rust postcard implementation. This may or may not matter depending on how sovereign you want Cosmix to be.

**Lua scripting layer.** Your ARexx-inspired orchestration includes Lua scripts driving cross-app behaviour. Lua reading postcard binary off a socket is painful. Lua reading JSON frontmatter is three lines.

## The Pragmatic Split

Rather than a binary choice, the mesh naturally stratifies:

| Layer | Traffic type | Format |
|---|---|---|
| compositor ↔ shell | Pointer, frame, surface events | postcard binary |
| intra-node service fast paths | Health, metrics, event streams | postcard binary |
| vector/embedding payloads | Float arrays | postcard binary |
| inter-service orchestration | Agent commands, JMAP events | AMP (JSON body) |
| node-to-node control plane | Config, topology, capability | AMP (JSON body) |
| Lua scripting interface | All orchestration | AMP (JSON body) |
| bulk data transfers | Log streams, file chunks | postcard or raw bytes |

The guiding principle: **if a human or a Lua script needs to read or write it, keep it AMP. If it's Rust-to-Rust on a hot path, postcard.** The boundary between those two zones is actually quite clean in Cosmix's architecture — it roughly maps to whether the message crosses the AMP router or stays within a single service's internal event loop.

One more consideration: if you ever want to expose AMP as the public extension API for Cosmix — so operators can write custom integrations without forking Rust — then keeping the control plane human-readable is a significant advantage. The compositor socket being binary is fine because nobody external should be talking to it directly.

***

Now I have the full picture. Rewriting it properly:Key changes from the first version:

**Structural:** §2 is now a dedicated "Protocol Architecture" section that documents the full pragmatic split as a first-class design decision, not buried in implementation detail. The module directory is `ipc/` throughout — `amp/` never appears inside the compositor.

**Dependencies:** `serde_json` gone, `postcard` in. The `debug-ipc` feature flag is now a first-class build feature, not an afterthought.

**§6 schema evolution rules** are new and important — postcard's positional encoding means a wrong field reorder silently corrupts data rather than failing loudly. Claude Code needs those rules in front of it.

**Phase 5** now explicitly notes that `cosmix-shell` has *two* sockets: the binary postcard one down to the compositor, and a separate AMP socket up to the mesh. That boundary needs to be clear from the start or it'll get muddled during implementation.

**§14** (collapsing the boundary) now makes explicit that the postcard socket becomes a `tokio::sync::mpsc` channel in the merged future — same message types, transport swaps underneath. That's a clean migration path.

