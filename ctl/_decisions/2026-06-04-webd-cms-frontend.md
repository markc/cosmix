---
title: webd CMS frontend — Mix-rendered HTML + self-hosted hypermedia; no compiled UI framework
date: 2026-06-04
status: Decided — sets the default frontend direction for the webd CMS; the public-site + first-party-admin path is binding, the SDUI and WASM-island items are deferred, and a Leptos web-surface exemption is explicitly reserved to Mark (§9)
draws_from:
  - "CLAUDE.md (Three Criteria; ARexx/ABP-Display-Protocol relationship)"
  - "_decisions/2026-06-04-maild-webd-trust-split.md"
  - "the retired amp-first-ui-architecture decision (superseded by 2026-07-18-amp-as-control-plane.md; git history)"
  - "_decisions/2026-05-12-cosmix-mail-attic.md (the Dioxus removal rationale)"
  - "_decisions/2026-05-20-substrate-first-service-pattern.md"
  - "cosmix-webd/src/mix_handler.rs (verified handler contract)"
scope: "cosmix-webd (the CMS frontend surface), cosmix-lib-mix (web-safety builtins). maild and disp-* unaffected."
---

# Cosmix ADR — webd CMS frontend: Mix-rendered HTML + self-hosted hypermedia; no compiled UI framework

**Status:** Decided. The public-site + first-party-admin rendering path is binding; SDUI and the WASM island are deferred (build only when forced); a web-surface exemption for a Rust UI framework is reserved to Mark (§9).
**Scope:** `cosmix-webd` (the CMS frontend), `cosmix-lib-mix` (web-safety builtins). `maild` and `disp-*` are unaffected.
**Audience:** Claude Code / Codex review sessions. Self-contained — it does not assume you saw the originating research.

---

## 1. Decision (TL;DR)

1. **The webd CMS frontend is Mix-rendered server-side HTML.** Each page/route is an operator- (or agent-) authored `.mix` handler under the vhost `www_dir`, served by the existing embedded-Mix path (`webd.handlers`, slice #3 of the trust-split ADR). The handler renders complete HTML and returns it as a `String` (or a typed `{status, headers, body}` map).
2. **Interactivity is one self-hosted hypermedia file** — **Datastar** as the primary include (~12 KiB; server-driven DOM swaps *and* client-side signals in one dependency), **htmx** as the battle-tested fallback. It is a single static `.js` vendored into `www_dir` and served by the existing `ServeDir`. **No bundler, no transpiler, no npm, no Node, no CDN.** The same handler returns a full page on a normal request and an HTML *fragment* when the hypermedia header is present.
3. **No compiled UI framework is the rendering layer.** Reject **React/Inertia** (JS + Vite build + Node SSR sidecar) and reject **Leptos/Sycamore/Perseus** and the compile-time Rust templaters (**Maud/Askama/Rinja/Hypertext/Sailfish**) as the page renderer. They bake the UI into a compiled artifact, so changing a page means a recompile + fleet redeploy — the inverse of webd's verified edit-`.mix`-file-and-the-page-is-live loop (§3). **Tera** (runtime Rust templates) is also rejected: it live-edits the skin but leaves page *logic* compiled in Rust, adding a second DSL and a dependency to do a worse-aligned version of what a Mix handler already does.
4. **A scoped WASM island is a deferred, quarantined escape hatch — not the frontend.** *If and only if* one genuinely-rich first-party admin widget (live WYSIWYG body editor, visual drag-drop page builder) provably exceeds hypermedia's ceiling, add a **single `wasm-bindgen --target web` custom element** — built as a *separate* static artifact dropped into `www_dir`, **not** `cargo-leptos`, **not** a dual-target Leptos app, so webd's own build never acquires a `wasm32` target. Build **none** for v1.
5. **Server-Driven UI (Mix emits a `{type, props, children}` tree) is the long-term shape for the agent-operable admin *app* surface, deferred.** It is the most substrate-coherent option and the true web analogue of the desktop ABP Display Protocol, but it means owning a bespoke UI framework and it is client-rendered (weak public SEO). Adopt it later for the authenticated admin app once the cheap builtin gaps land — never as the v1 public-CMS default.

**One-language invariant:** the entire CMS renders in **one** language — Mix — for both the trusted first-party surface (in-process embed, live today) and, when slice #4 lands, untrusted tenant templates (out-of-process pooled workers). The hypermedia client is dumb HTML-over-the-wire with zero client trust surface.

---

## 2. Why (the reasoning)

The decisive axis is **not JavaScript vs Rust. It is compiled UI vs live UI**, and webd already chose live. Three structural arguments, in priority order:

**(a) A compiled UI framework breaks Criterion #2 (agent-modifiable).** webd's embedded-Mix handler caches the parsed AST by file **mtime** and re-parses on change (`mix_handler.rs::get_or_parse`; test `ast_cache_reuses_until_mtime_changes`). The agent loop to change a page is: edit one `.mix` text file → the next request re-parses → the page is live. **No recompile, no restart, no redeploy, no daemon-lifecycle event.** A Rust UI framework (Leptos/Sycamore) compiles the UI into *both* the server binary (`ssr` feature) *and* a separate `wasm-bindgen`/`wasm-opt` WASM artifact (`hydrate` feature); to change any page you edit `.rs`, rebuild the 27-crate `cos` workspace under `lto=true`, redeploy a new binary **and** new WASM to the fleet, and restart the service. The mandate's Criterion #2 explicitly prizes "change via structured channels … not ad-hoc file edits … runtime-updatable." A compiled UI is the textbook violation. The compile-time Rust templaters (Maud/Askama) fail the same way; Tera half-escapes it (templates reload, logic recompiles), which is still worse than a Mix handler that holds logic *and* markup in one live-editable file.

**(b) It re-opens a precedent the project already settled the other way, with no forcing reason.** On 2026-04-09 the 18 Dioxus crates were deleted (`2026-05-12-cosmix-mail-attic.md`) on a *structural* argument, not an incidental one: "A Dioxus app is opaque to [the agent-operability] goal: its widget tree lives in browser DOM / platform-webview state, not in ABP messages." The replacement puts "all UI state on the wire by construction." That critique transfers verbatim to a Leptos island: its reactive signal graph lives in a compiled WASM runtime in the browser — unobservable and unmodifiable by an agent as structured data. The standing `feedback_no_dioxus` rule and the ABP Display Protocol direction (the retired amp-first-ui-architecture decision — superseded 2026-07-18 by amp-as-control-plane; git history) are the desktop expression of a substrate-wide stance: **UI is server-driven data, not a compiled component tree.** Re-introducing the same paradigm on the web needs an extraordinary reason; the research found Leptos's only unique wins over Mix+hypermedia are "typed-component legibility" and "React-like DX without npm" — neither a substrate requirement, both dispensable for a content CMS.

**(c) A Rust UI framework is redundant where it actually matters (multi-tenant).** The binding trust-split ADR makes the in-process embed **trusted/operator-authored only**; untrusted tenant code must run in the pooled out-of-process worker tier (slice #4). A tenant fundamentally **cannot** ship Rust to be compiled into your daemon — that would be letting a tenant trigger a recompile of webd. So every untrusted page **must** run through an interpreted/sandboxed path (Mix in a pooled worker). That confines Leptos to first-party pages the trusted Mix embed *already* serves hot-reloadably today — it adds a second language and a recompile boundary to duplicate what Mix already does, while contributing **nothing** to the hard multi-tenant surface.

**Against all three, Mix-rendered HTML + a single self-hosted hypermedia file is Pareto-best across the Three Criteria:** npm-free, build-step-free, fully server-rendered for SEO, runtime-editable by an agent with no recompile, and it consumes exactly the single buffered HTML response a Mix handler already returns. It is the web analogue of "the desktop is ABP + markdown, no GUI framework."

---

## 3. What the codebase shows (grounding facts — verified 2026-06-04)

- **`cosmix-webd` v0.4.0** is the 443 front door + ACME/cert authority + static serving + embedded-Mix CMS handlers (axum; links bundled SQLite and `ring`).
- **Handler contract** (`mix_handler.rs`): a `.mix` script under a vhost `www_dir`, dispatched by the SPEC-12 `webd.handlers` namespace on `(vhost_fqdn, method, path_pattern) → handler_ref`, receives globals **`METHOD`, `PATH`, `QUERY`, `HOST`, `BODY`** (utf8-lossy, 1 MiB cap) **, `HEADERS`** (lowercased map). It returns either a `String` (⇒ 200 `text/html`) or a `Map {status?, headers?, body?}` (typed; status clamped 100–599, arbitrary header map, string/bytes body). Anything else falls back to captured stdout (200) or 204. **This contract already expresses every hypermedia and every Inertia wire rule** — full-page vs fragment, status, headers, redirects — with no new daemon capability.
- **Hot reload by mtime** (`get_or_parse`): the parsed AST is cached per path and re-parsed when the file's mtime changes. Edit the file → next request serves the new page. No build, no restart.
- **Sandbox** (trusted embedded path): capability policy **Pure + FsRead by default** (FsWrite / Network / Process / Env denied — covering builtins *and* shell syntax `sh`/`$()`/pipe; `sleep` denied); **recursion cap 16**; **5 s** wall-clock deadline; collection caps. No ABP handler installed (`send`/`emit` inert). A route may **opt into `capabilities = ["db"]`** (the only token webd currently knows), which adds the `Db` class and injects the per-vhost SQLite connection — such a handler reads **and writes** the per-vhost DB via `db_query` / `db_exec` (DDL + DML; verified by the `db_route_writes_then_reads_200` test). Without the `db` token those builtins error cleanly. So a handler can read files, read/write the per-vhost DB when db-capable, build/parse JSON, and template — it still **cannot** write arbitrary files, open sockets, run shell, or reach ABP. **First-party writes have a Mix path today** (a `db`-capable handler); the native `/api/posts*` routes remain for non-Mix callers.
- **Mix web builtins present** (cosmix-lib-mix): `json_encode`, `json_parse`, `template`, `markdown_escape`, `sanitize`, `url_parse`, `url_encode`, `url_decode`, `format`, heredocs; plus `db_query` / `db_exec` (active only inside a `db`-capable handler). **`json_encode` does NOT HTML-escape** (`</script>`, `<`, `&`, U+2028/U+2029 pass verbatim) and **there is no `html_escape` builtin** — `markdown_escape` is the wrong escaper for HTML element/attribute context. This is the one security blocker (§6).
- **Single buffered response:** `MixResponse` carries a `body: String` built in one pass; there is no yield/generator, so **SSE/streaming is a genuine gap** (§6, deferred).
- **Existing minimal CMS:** a `posts` SQLite table (`id, slug, title, content, published, created, updated`) + native `/api/posts*` JSON routes, gated by a per-vhost `has_cms` flag. `www_dir` is served via `ServeDir` *after* the handler table is consulted.

---

## 4. The option field considered

| Approach | npm | Change → live loop | SEO | Agent-modifiable | Disposition |
|---|---|---|---|---|---|
| **Mix + hypermedia (Datastar/htmx)** | **none** | edit `.mix` (live) | strong | high | **Chosen default** |
| + scoped WASM island (deferred) | none | 95% live / 5% recompile | strong | high | **Target architecture** — build the island only when one widget forces it |
| Mix SDUI (component-tree as data) | optional | edit `.mix` (live) | weak (client-rendered) | highest | **Deferred** — long-term admin-*app* shape; means owning a framework |
| Tera (Rust runtime templates) | none | template live / **logic recompiles** | strong | low | Rejected — strictly worse than a Mix handler |
| Leptos / Sycamore islands | optional | **recompile + fleet redeploy** | strong | lowest | Rejected as renderer; at most the scoped island above |
| Maud / Askama (compile-time) | none | **recompile + fleet redeploy** | strong | lowest | Rejected — UI baked into the binary |
| React + Inertia | build-time | Vite rebuild + **Node SSR sidecar** | needs Node SSR | lowest | Rejected — see below |
| Decoupled React SPA + JSON API | build-time | Vite rebuild | poor (client-only) | low | Not the default; the least-bad React answer if a hard "must be React" admin constraint ever appears |

**On Inertia specifically (the originating question):** an Inertia-on-Mix bridge is *technically feasible today* — the `{status, headers, body}` map + `json_encode` already cover the entire Inertia server-adapter wire protocol (initial-load `data-page`, `X-Inertia` XHR visits, 409 + `X-Inertia-Location`, 303 redirects, partial reloads). But "write the majority of the frontend in Mix" is **impossible with Inertia by construction**: Inertia keeps 100% of components/pages/forms in JSX and moves only the ~15–25% controller layer to the backend. It also forces a Vite build, hits the `json_encode` XSS gap on the inline `data-page`, and — for a CMS's SEO — requires a **separate persistent Node SSR process** (the handler sandbox denies Network + Process), the heaviest violation of the single-front-door / no-runtime-Node posture. **Feasible, but the wrong tool for the headline goal and a substrate-misaligned default.** Documented here so it is not re-proposed without re-reading this section.

---

## 5. Target architecture

**Two surfaces, one language, behind webd's single 443 front door**, wired through the `webd.handlers` SPEC-12 props table (the route map is a structured row an agent edits via `props.set`, not a file convention).

**Public content site (load-bearing, fully aligned).** Each page is a `.mix` handler rendering complete server-side HTML from the `posts` table + content files (FsRead + the per-vhost DB seam — `db_query` reads, `db_exec` writes when the route opts into the `db` capability). Full markup on the wire ⇒ native SEO + fast first paint, no hydration. The hypermedia `.js`, images, and CSS come from `www_dir` via `ServeDir` — air-gapped on the WG mesh, no CDN.

**First-party admin.** The *same* handlers return HTML **fragments** that the single hypermedia script swaps in — inline edit, partial swaps, debounced search, form validation, modals, infinite scroll — all from the single buffered response a handler already returns. A handler reads the hypermedia request header (`hx-request` / Datastar equivalent) from `HEADERS` and branches: full page vs fragment; it sets redirect/trigger headers via the headers map.

**Trust mapping (consistent with the trust-split ADR).**
- *Trusted* = public site + first-party admin, authored by Mark/agents, run **embedded in-process** under the verified Pure + FsRead sandbox (+ an opt-in per-vhost `Db` capability for read/write SQL). **Live today.**
- *Untrusted* = future tenant-authored templates → the **same language (Mix)** but the **pooled out-of-process worker tier** (slice #4, not yet built). The interpreted-and-sandboxed model is exactly what an out-of-process worker hosts; a compiled framework cannot serve this surface at all.
- The hypermedia client is dumb HTML-over-the-wire — zero client trust surface.

**Escape hatch (deferred, quarantined).** One genuinely-rich first-party admin widget, if it ever provably exceeds hypermedia, becomes a single `wasm-bindgen --target web` custom element built as a *separate* static artifact in `www_dir`. webd's own build never gains a `wasm32` target; the recompile cost is isolated to that one widget crate. The island writes its result to a hidden form input and emits DOM events the hypermedia layer converts to server exchanges. **First-party trusted admin only — never a tenant surface, never the public site.**

---

## 6. Required substrate work (Mix-first; consequences of this decision)

These are work items, not blockers on the architecture. All land in `cosmix-lib-mix` (bump `cosmix-lib-mix` + `cosmix-mix` in lockstep) unless noted.

1. **`html_escape` builtin — HIGHEST PRIORITY, security blocker.** Context-correct escaping of `<`, `>`, `&`, `"`, `'` (and ideally a `html_attr_escape` / U+2028–U+2029-aware variant for inline-JSON contexts). `markdown_escape` is the wrong escaper; `json_encode` does not HTML-escape. **No admin or user-content surface ships before this lands.**
2. **`parse_query` / `parse_form` (structured)** — `url_parse` / `url_decode` / `url_encode` already exist, but there is no one-call parse of a raw query string or a form-urlencoded `BODY` into a map; handlers receive `QUERY` and `BODY` raw. Highest-leverage non-security addition for any form POST or search/filter.
3. **Cookie/session helpers** (read `Cookie` into a map; the headers map already emits `Set-Cookie`) **+ `hmac_sha256`** *or* opaque server-side session IDs (`uuid`/`random_password` already suffice) — for admin auth + CSRF. Do **not** use `hash_sha256(secret .. msg)` (length-extension-weak).
4. **Multipart/form-data parsing** — for media uploads (or handle uploads on a native route initially).
5. **Layout/partials ergonomics — Phase-0 spike (the one load-bearing risk).** Confirm Mix `template()`/heredoc + `include` + HOF helpers express a shared layout, partials, and inheritance cleanly at CMS scale. If they don't, the Mix-first fix is to **add a layout/partial capability to Mix**, not import Tera. Note: `template()` is single-brace `{key}` and does **not** auto-escape — correct the AGENTS.md / builtin help text, which wrongly say double-brace.
6. **Writes** — *not a gap.* A handler that opts into `capabilities = ["db"]` persists to the per-vhost SQLite via `db_exec` (DDL + DML) today; the native `/api/posts*` routes remain for non-Mix callers. (Multi-tenant *untrusted* writes still wait for the pooled-worker tier, §5.)
7. **SSE/streaming** (deferred decision gate) — `MixResponse` is single-buffered. Only if live multi-push UI is genuinely wanted: either a native axum `text/event-stream` route bypassing Mix, or a new Mix-handler streaming contract. Not needed for plain hypermedia swaps.

---

## 7. Phased plan

- **Phase 0 — Mix layout/partials spike (gates the plan).** Prove a shared layout + partials + a couple of page templates are ergonomic in pure Mix. If painful → extend Mix (not Tera). This is the single open risk that could change the approach.
- **Phase 1 — `html_escape` + the request-parser builtins** land first; they unblock every later surface safely.
- **Phase 2 — Public content site** as pure Mix handlers rendering full SSR HTML from the `posts` table. SEO-complete, no client JS. Vendor one hypermedia `.js` into `www_dir` but don't wire it on the public side yet. Independently valuable.
- **Phase 3 — First-party admin** with the hypermedia layer wired (Datastar primary / htmx fallback): inline edit, fragments, validation, modals. Writes via a `db`-capable handler (`db_exec`) or native `/api/posts*`.
- **Phase 4 — Cookies/session/CSRF** builtins for admin auth.
- **Phase 5 (gated) — Multi-tenant** waits for the pooled out-of-process Mix worker tier (slice #4). Until then: single-tenant / operator-authored, or tenants get a static SPA + native `/api` with **no** tenant server-side code.
- **Phase 6 (deferred, conditional) — WASM island** for the one widget that proves it can't be hypermedia. Build nothing in WASM before this is forced.

---

## 8. Consequences / what this rules out

- **No npm, no Node, no bundler in the CMS** — end to end. The single avoidable concession is Tailwind-via-npm for styling; use the standalone Tailwind binary or hand-written CSS instead.
- **No compiled UI framework as the renderer** (React/Inertia, Leptos/Sycamore/Perseus, Maud/Askama/Tera). A page change never requires a webd rebuild + fleet redeploy.
- **One rendering language** across trusted and (future) untrusted surfaces — the substrate's declared primary control surface, agent-legible by construction.
- **Honest costs accepted:** no compile-time template typing (errors surface at request time — acceptable for content that recovers by another edit); the SSE-streaming gap; layout-composition ergonomics unproven until the Phase-0 spike; richest client widgets (WYSIWYG/drag-drop) need the deferred island. These are bounded and named, not hidden.

---

## 9. Open questions / escalation

- **Web-surface exemption (Mark's call).** This ADR treats the web frontend as bound by the same UI-as-data precedent that removed Dioxus. If Mark explicitly **exempts** the web surface, a scoped Leptos *admin* story becomes more defensible — but that exemption is an architecture-boundary decision reserved to him, and the redundancy/recompile arguments against *full* Leptos still hold even with an exemption. The default in §1 stands until such an exemption is granted.
- **Datastar vs htmx** as the primary include — Datastar collapses server-driven swaps + client signals into one ~12 KiB file (closest to "UI as data"); htmx is the lower-risk, more battle-tested choice. Either is a single self-hosted file, so the pick is cheap and reversible. Default Datastar, fall back to htmx if battle-testing matters more on first contact.
- **SSE/streaming contract** — native axum route vs a Mix-handler yield capability — decided only when a real live-push need appears (Phase 4 gate, §6.7).
- **Multi-tenant timing** — gated entirely on the pooled-worker tier (trust-split slice #4); not solved here.

---

## 10. Decided against — do not re-litigate

- **A Rust UI framework (Leptos/Sycamore/Perseus) as the webd renderer** — breaks Criterion #2 (recompile-to-change-a-page), re-opens the Dioxus removal with no forcing reason, and is redundant on the multi-tenant surface (tenants can't ship Rust). Reserve at most a single scoped WASM island (§5).
- **Compile-time Rust templaters (Maud/Askama/Rinja/Hypertext/Sailfish) for pages** — UI baked into the binary; same recompile objection.
- **Tera (runtime Rust templates)** — live skin but compiled logic + a second DSL; strictly worse than a Mix handler that holds logic and markup in one live-editable file. Reach for it only if the Phase-0 Mix-layout spike fails — and even then prefer extending Mix.
- **React + Inertia with SSR** — forces a persistent Node SSR process + native Rust proxy in webd (sandbox denies Network/Process); 3 runtimes and a second non-Cosmix daemon outside webd's sandboxed, ABP-controllable model.
- **Running tenant-authored templates through the embedded path** to ship multi-tenant sooner — re-merges the exact trust boundary the trust-split ADR exists to create (in-process capability-gating cannot contain a compromise in the address space holding webd's TLS keys). Tenant code waits for pooled workers or is reduced to static-files-only.
- **Inlining any user/DB content with raw `json_encode` or `markdown_escape`** — a verified XSS hole until `html_escape` lands.

---

*Drafted by Claude 2026-06-04 from a two-workflow research + adversarial-evaluation pass (Inertia/SDUI/hypermedia track + Rust-native/Rust-SSR track, each with independent option judges and a hostile skeptic). The compiled-vs-live axis, the mtime hot-reload loop, the `json_encode` no-HTML-escape gap, and the handler contract were verified against `cosmix-webd/src/mix_handler.rs` and the live `mix` binary. The binary and the code are the oracle — when this ADR conflicts with them, they win; fix this doc in the same change.*
