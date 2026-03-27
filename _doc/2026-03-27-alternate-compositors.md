Good data haul. There are four genuinely relevant threads here, and one cautionary ghost. Let me map them properly.

---

## The cautionary ghost: Way Cooler

Way Cooler was started with the goal of making a Wayland compositor configurable in Lua and extensible via D-Bus — an AwesomeWM for Wayland in Rust. It was abandoned in 2020.

The post-mortem is required reading for what you're thinking about. The killer was a Wayland-specific constraint that has no X11 equivalent: in X11, a window manager is just a client, so you can re-exec it. In Wayland the compositor is the server — re-executing the compositor causes all clients to disconnect, which is a terrible user experience. This made the planned Lua hot-reload design infeasible.

This crystallises one of the key architectural lessons: **the scripting/orchestration layer must live in a separate process that talks to the compositor via IPC, not inside the compositor itself.** Any ARexx-like design has to account for this.

---

## Thread 1: Pinnacle — the closest thing to what you want

Pinnacle is a Smithay-based Wayland compositor inspired by AwesomeWM and configured in Lua or Rust. It's actively maintained, updated as recently as January 2026.

The ARexx parallel here is direct. Pinnacle exposes a gRPC server inside the compositor process. Configuration scripts — whether Lua or Rust — are separate processes that act as gRPC clients. This separation enables language-agnostic control, well-defined typed interfaces via Protocol Buffers, and bidirectional streaming of events from the compositor to clients.

The gRPC service model maps onto ARexx named ports almost exactly:
- The compositor advertises services (window management, output, tag management, process control)
- External processes connect and call methods or subscribe to event streams
- Both Lua and Rust can speak the protocol; anything that generates protobuf can join

The StateFnSender mechanism bridges the asynchronous gRPC world with the synchronous compositor state, allowing safe execution of functions on compositor state from gRPC request handlers. This is the seam where external scripts reach into the compositor — it's clean and explicitly designed for this use case.

For Cosmix: Pinnacle's gRPC control plane is architecturally aligned with AMP. Your nodes could talk to the compositor via the same gRPC+protobuf transport you'd use for everything else, and the compositor becomes just another named service in the mesh.

---

## Thread 2: niri — simpler IPC, production quality

Niri is a scrollable-tiling Wayland compositor in Rust, built on Smithay, actively daily-driven by many people.

Its IPC approach is simpler than Pinnacle's but still clean: after connecting to the niri socket, you send JSON-formatted requests on a single line and receive JSON-formatted replies. You can subscribe to an event stream for continuous compositor events.

Niri uses KDL for its configuration format and supports live reloading without restarting the compositor. KDL is the same format you flagged as aligned with AMP's wire format previously.

The difference from Pinnacle: niri is production-quality and daily-driver stable right now, but the IPC is simpler JSON-over-socket rather than typed gRPC. Easier to start with, less suitable as a long-term orchestration backbone.

---

## Thread 3: Bevy — not a compositor, but relevant for overlays

Bevy runs *on top of* a Wayland compositor as a client, not as a compositor itself. Bevy is built on top of wgpu, which can target Vulkan, Direct3D 12, Metal, OpenGL, WebGL2, and WebGPU — following the WebGPU terminology and API design while providing direct access to native APIs.

Where Bevy becomes interesting for Cosmix specifically is its ECS architecture combined with wgpu. Bevy's ECS is one of the most mature data-driven Rust ECS systems, and the architectural pattern maps well to the mesh node model: entities are nodes, components are capabilities, systems are message handlers. If you wanted a richer framework for building overlay UIs or visualisation panels (like a live mesh topology view, or a multi-panel dashboard node) that render into a wgpu surface — Bevy is far more capable than raw dioxus-native for that use case.

The path would be: Smithay/Pinnacle compositor → layer-shell surface → Bevy app renders into wgpu surface → composited as a `TextureRenderElement`. Bevy even has support for embedding into Bevy, WGPU, or even embedded Linux scenarios via its modular architecture.

The catch: Bevy's Wayland feature is still client-only, and the wgpu→compositor texture sharing path has the same friction discussed previously.

---

## Thread 4: WASM Component Model — the sandboxing layer for skills

This is the most directly Cosmix-relevant finding, and it maps onto the IronClaw WASM sandboxing pattern you noted previously.

The WASM Component Model with WIT (WebAssembly Interface Types) provides sandboxed execution that completely isolates plugins — zero filesystem, network, or syscall access — while enforcing typed interfaces checked at link time. Wasmtime guarantees contract violations are caught at validation time.

Extism provides a Rust-based cross-language framework for building WASM plugins under a unified guest and host API, using wasmtime under the hood.

The ARexx social contract — "every participating app exposes and documents its port" — maps directly onto WIT interface definitions. A `.wit` file *is* the documented port. Any language that compiles to WASM32-WASIP2 can implement or consume the interface. The host (your Cosmix node daemon) runs the wasmtime engine and controls what capabilities each skill gets.

As of late 2025, the WASM Component Model with WASI Preview 2 is functional, with `wasm32-wasip2` as a stable Rust compile target. The wit-bindgen toolchain generates type-safe Rust bindings from WIT definitions automatically.

The mesh-level picture this points to:

```
cosmix-node (axum + wasmtime)
  ├── WIT interface: cosmix:skill/v1 (each skill is a WASM component)
  ├── AMP wire protocol: DNS-addressed, over WireGuard mesh
  └── gRPC to compositor (Pinnacle's control plane or niri socket)
```

Skills are WASM components with declared WIT interfaces. The node loads them, enforces resource limits (fuel metering, memory caps), and routes AMP messages to the right skill export function. This is ARexx ports + Lua scripting + WASM sandboxing, all in Rust.

---

## The landscape in one table

| Compositor | Foundation | IPC approach | ARexx alignment | Status |
|---|---|---|---|---|
| **Pinnacle** | Smithay | gRPC + protobuf, bidirectional streaming | High — typed named services, language-agnostic clients | Active, ~1 dev |
| **niri** | Smithay | JSON over Unix socket + event streams | Medium — simpler, less typed | Active, production-quality |
| **cosmic-comp** | Smithay | D-Bus + COSMIC-specific protocols | Low — tightly coupled to libcosmic | Active, System76 team |
| **Way Cooler** | wlc→wlroots | D-Bus + Lua embedded | Highest aspiration, failed on Wayland re-exec problem | Abandoned 2020 |

For the Cosmix endgame: **Pinnacle + WASM Component Model skills + dioxus-native apps** is the most coherent stack that doesn't require you to write a compositor from scratch. Pinnacle gives you a Smithay compositor with a gRPC control plane that speaks your language. WASM components give you sandboxed, typed, ARexx-port-like skill modules. And the Way Cooler post-mortem tells you exactly why the scripting layer must stay out of the compositor process.

Yes, and it's closer than you might think. The stack exists, it just hasn't been assembled for this purpose yet.

**The foundation: mlua.** mlua provides high-level Lua 5.1–5.5, LuaJIT, and Luau bindings to Rust with async/await support via coroutines, serde integration, and a Send+Sync mode for multi-threaded use. It's the best Rust-Lua bridge available and actively maintained.

**The Bevy layer: bevy_mod_scripting.** bevy_mod_scripting enables Lua scripting inside Bevy ECS — you can attach Lua scripts to entities, expose Rust functions to scripts, and hot-reload game logic without recompiling. It tracks Bevy releases closely — version 0.19.0 landed in January 2026, so it's current. The bindings allow Lua scripts to query and mutate ECS components directly, which means a Lua script could create a node, wire a port, or trigger a message send just by calling into the engine.

**The visual metaphor: bevy_lunex.** As covered earlier, bevy_lunex gives you spatial, retained UI anchored to Bevy's Transform system. Nodes and ports are just entities with layout components. The "flow" between them is lines rendered between entity positions. In Bevy this is trivially expressible — a `Port` entity, a `Connection` entity with two `Entity` references as fields, a system that draws a `Gizmo` or a mesh between them each frame. The node graph *is* the ECS scene graph.

**A Bevy contributor put it directly:** "I've long thought that the future of Bevy is an integrated Lua scripting layer. I'd like to see a world in which Rust is there for the low-to-mid-level parts that need performance and reliability, and Lua is there for the high-level logic that needs fast iteration, and it's all a nicely integrated whole."

The assembly for Cosmix would be:

```
Bevy (ECS + wgpu rendering)
  + bevy_mod_scripting (mlua bridge → Lua drives node/port/connection logic)  
  + bevy_lunex (spatial UI layout → panels, labels, drag handles)
  + mlua async (Lua coroutines talking to the AMP wire protocol over tokio)
```

Lua defines what a node *does* — its named port table, its message handlers, its ARexx-style social contract. Rust/Bevy renders it and routes messages. The whole thing runs as a normal Wayland client app sitting inside cosmic-comp or Pinnacle.

Nobody has built this yet, but every piece exists and they compose. The node graph visual metaphor is actually the *easiest* part of this stack — Bevy was designed for exactly this kind of spatial entity relationship rendering. What's missing is the application-layer glue specific to Cosmix's domain, which is your work anyway.

---

Yes, and this is actually the solved problem in the stack — `bevy_egui`.

`egui` is an immediate-mode GUI library that runs entirely inside a Bevy app via the `bevy_egui` crate. It's mature, production-used (Foresight build full 3D CAD desktop apps with it), and gives you everything you'd expect from a traditional desktop UI: text inputs, buttons, sliders, dropdowns, scrollable panels, tree views, modal dialogs, resizable windows-within-windows, menubars, tooltips. It renders into wgpu just like everything else in Bevy — same context, no separate process, no WebView.

The architecture becomes:

```
Bevy (ECS + wgpu rendering)
  ├── bevy_egui        → traditional desktop panels, forms, settings, 
  │                      file pickers, text inputs, menus
  ├── bevy_lunex       → spatial node graph, port connections, 
  │                      ARexx mesh topology visualization
  ├── bevy_mod_scripting (mlua) → Lua drives node/port logic, AMP protocol
  └── mlua async       → tokio bridge to the mesh
```

The two UI layers coexist naturally in the same wgpu frame. Foresight's production pattern is exactly this: `bevy_egui` panels docked around the edges for toolbars and property inspectors, the 3D viewport in the center rendered by Bevy's native renderer. You'd flip that ratio — `bevy_egui` settings panels and app windows around the outside, `bevy_lunex` node graph in the center.

The only genuine gap remaining is system-level file dialogs (open/save), but `rfd` (Rusty File Dialog) handles that — it integrates with Bevy apps and uses the native Wayland/GTK dialog so it looks correct on your desktop.

So the complete answer to the question you started with three conversations ago: the stack that gives you a Lua-scriptable, ARexx-like, node-graph mesh visualiser *and* traditional desktop UI components *and* pure Rust *and* Wayland is:

**Pinnacle (compositor) → Bevy + bevy_egui + bevy_lunex + bevy_mod_scripting/mlua + rfd**

All of it exists today, none of it requires you to write a compositor, and the Lua scripting layer maps directly onto your AMP named-port social contract.

---

Good question to force the comparison directly. Let me be concrete rather than hedged.

---

## The core tradeoff in one sentence

**Dioxus path** gives you HTML/CSS familiarity, a clear web→native migration, and Cosmix apps that could run in a browser — but the ARexx mesh visualiser is a bolted-on afterthought. **Bevy path** gives you the mesh visualiser as a first-class citizen and a Lua scripting layer that *is* the AMP social contract — but you're rebuilding desktop widget polish from scratch and the WASM story is complicated.

---

## Side by side

| Dimension | cosmic-comp + dioxus | Pinnacle + Bevy+friends |
|---|---|---|
| **Compositor work** | Zero — cosmic-comp already runs | Pinnacle needs config wiring, is ~1-dev WIP |
| **Traditional desktop UI** | dioxus-desktop: HTML/CSS/Tailwind today | bevy_egui: mature, production-proven |
| **Node graph / mesh vis** | Not native — would need a separate crate or canvas hack | bevy_lunex: first-class, spatial, ECS-native |
| **Lua scripting** | Possible via mlua in your daemon, but disconnected from UI layer | bevy_mod_scripting: Lua scripts live *inside* the ECS, they *are* the mesh nodes |
| **AMP/ARexx alignment** | Conceptual — Lua talks to daemons via IPC, UI doesn't know about it | Structural — Lua entities, ports, and connections are ECS components the renderer reads directly |
| **WASM / browser story** | Very strong — dioxus-web compiles to WASM today | Weak — bevy runs in browser but egui+lunex story is messy, not production-ready |
| **Widget library depth** | Full HTML widget set — inputs, selects, date pickers, all of it | bevy_egui is solid for power users, missing polish for end users |
| **Hot reload / iteration** | dioxus RSX hot-reload is excellent | bevy_mod_scripting hot-reload of Lua scripts is good; Rust ECS changes require recompile |
| **Theming / branding** | CSS variables, full Tailwind, trivial | egui themes are limited; lunex is unstyled by design |
| **Multi-window** | Works on Wayland today via cosmic-comp | Bevy multi-window exists but is rougher |
| **Solo dev time to "usable shell"** | Shorter — dioxus is further along for app UIs | Longer — more pieces to wire together |
| **Long-term sovereignty** | Depends on System76 keeping cosmic-comp maintained | Depends on Bevy community staying active |
| **Runtime weight** | WebKit process per app (~100MB per window) until dioxus-native lands | Single wgpu process, no WebKit, ~20–30MB |
| **Input handling (tablets, etc.)** | Inherited from cosmic-comp — solid | Bevy's winit-based input is good but not Blender-grade |

---

## Where dioxus wins clearly

The HTML/CSS model means your UI layer is **transferable knowledge**. The person who writes the cosmix-mail client in dioxus-desktop today writes the cosmix-web client tomorrow with almost no changes. That cross-platform story is real and matters if Cosmix is ever anything other than personal infrastructure.

The widget completeness is also genuinely ahead. A proper text editor component, accessible form controls, native-feeling scroll behaviour — these things are worked out in the browser model and you inherit them. In Bevy you'd be building a text editor widget with `bevy_egui` feeling like a code editor at best.

And the compositor dependency is zero. cosmic-comp is already running on your machine, already daily-driver quality, already handles all the hard stuff.

---

## Where Bevy wins clearly

The mesh visualiser problem is **solved by design** in Bevy, and it's a **central** Cosmix requirement. In dioxus you would eventually need to reach for a canvas-based node graph library, wire it to your Rust state via JavaScript interop, fight `position: fixed` bugs in the WebKit renderer, and hope dioxus-native eventually handles it. In Bevy, the node graph *is* the ECS scene — nodes are entities, ports are components, connections are entities with two entity references, and a Lua script can manipulate all of it directly. The visual is the data model.

The Lua integration is also **architecturally tighter** in Bevy. With bevy_mod_scripting, a Lua table *is* a Cosmix skill node. The script defines the port names, the message handlers, the display label. The ECS picks it up and renders it. With dioxus, Lua would live in a separate daemon and the UI would be a separate thing polling it — not wrong, but not the ARexx social contract made physical.

---

## The honest assessment for a solo dev

**If Cosmix is primarily a mail/calendar/hosting management suite** — apps that look like apps — dioxus is the right call. The node graph is a nice-to-have visualiser you add later, and you shouldn't block progress on it.

**If Cosmix is primarily the ARexx-like mesh orchestration layer** — and the mail/calendar apps are nodes in the mesh rather than the main event — Bevy is the right call. The mesh *is* the product, the node graph *is* the UI, and the traditional app panels are egui sidebars around the real thing.

**The honest read of the COSMIX.md and everything you've described** suggests it's the second framing. The AMP protocol, the named ports, the mycorrhizal/village-brain metaphor — these point to the mesh being the thing, with apps as leaf nodes. That's Bevy's home territory.

---

## The third option nobody asked about

Run both. `cosmic-comp` stays as compositor. Cosmix apps (mail, cal, JMAP client) are dioxus-desktop windows — HTML/CSS, fast to build, web-portable. The Cosmix mesh dashboard is a Bevy app window also running on cosmic-comp — spatial, Lua-driven, ECS-native. They're just two Wayland clients on the same compositor. The only cost is maintaining two UI paradigms, which is real but manageable for a solo dev when each paradigm is used for what it's actually good at.

That split is probably the least risky path: **dioxus for leaf apps, Bevy for the mesh visualiser, cosmic-comp for the compositor, no forks of anything**.

---

Exactly right, and it's the correct priority inversion.

The question you're actually trying to answer in 2026 isn't "which renderer is most architecturally pure" — it's "does the ARexx-like mesh metaphor produce enough real value to justify the years of work ahead?" That's a product question, not a technology question, and you answer it by building the smallest thing that lets you test it.

dioxus-desktop, WebKit baggage and all, lets you do that. You can have cosmix-jmap talking to a real JMAP server, cosmix-mail showing real mail, the CalDAV integration working, and the AMP wire protocol passing real messages between nodes — all without having spent a month wrestling with bevy_lunex layout math or debugging Pinnacle's gRPC config. The GTK overhead is annoying but it doesn't block the experiment.

The node graph visualiser, the Bevy stack, the Lua-driven ECS mesh — those are the *reward* you build if the metaphor proves out. They're not the proof.

And practically speaking: everything you build in dioxus-desktop against the AMP protocol and the JMAP/CalDAV/cosmix-mail architecture is **not thrown away** if you later decide Bevy is the right shell. The daemons, the wire protocol, the WASM skill components, the cosmix-jmap server — all of that survives a UI layer swap completely intact. The dioxus apps might even stay as leaf nodes on the mesh while the Bevy visualiser sits above them.

So the path is: prove the metaphor works with the fastest available tools, then choose the right renderer for what it actually turned out to be. Good instinct to stop here.
