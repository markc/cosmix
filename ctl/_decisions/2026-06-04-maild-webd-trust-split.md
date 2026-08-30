---
title: No single `vhostd`; split maild/webd by trust into embedded vs pooled-worker Mix execution
date: 2026-06-04
status: Decided (binding) — supersedes the cosmix-vhostd single-binary vision
supersedes:
  - "_plan/2026-06-03-cosmix-vhostd.md"
draws_from:
  - "CLAUDE.md"
  - "_decisions/2026-05-20-substrate-first-service-pattern.md"
  - "_plan/2026-06-03-cosmix-vhostd.md"
  - "project_mix_embed_sandbox_findings (memory)"
scope: "cosmix-maild, cosmix-webd, cosmix-lib-mix, the Mix execution model. disp-* unaffected."
---

# Cosmix ADR — No single `vhostd`; split maild/webd by trust into embedded vs pooled-worker Mix execution

**Status:** Decided (binding). Supersedes the earlier `cosmix-vhostd` single-binary vision.
**Scope:** `cosmix-maild`, `cosmix-webd`, `cosmix-lib-mix` (the `mix` repo), and the Mix execution model. `disp-*` surfaces unaffected.
**Audience:** Claude Code / Codex review sessions. This brief is self-contained — it does not assume you saw the originating discussion.

---

## 1. Decision (TL;DR)

1. **We will NOT build a single combined `cosmix-vhostd` binary** that folds `maild` + `webd` + an embedded Mix interpreter into one process. The single-daemon vision was the preferred aesthetic (one tiny VM/CT/VPS serving mail + web vhosts); it is set aside **deliberately**, not provisionally. Do not re-open it without a forcing reason that co-residence cannot satisfy.
2. **`maild` and `webd` remain separate processes** — two independently sandboxed, independently restartable systemd units.
3. **Mix gains a second execution mode.** Going forward there are two, chosen by the **trust level of the script, not the identity of the daemon**:
   - **Embedded** — *trusted, in-host, fast.* In-process, for operator/self-authored scripts.
   - **Pooled-worker (FPM-style)** — *untrusted, out-of-process, sandboxed.* A pool of separately-`exec`'d worker processes, for customer-uploaded scripts.
4. **Parts of `maild` and `webd` will likely be split** along this embedded/pooled-worker line as untrusted-script surfaces appear. The pooled-worker tier **augments** embedding; it does not replace it.

---

## 2. Why (rationale)

The merge was attractive for **deployment and management simplicity**. That simplicity **collapses the moment the daemons face multi-tenant workloads on non-WG-mesh (i.e. public, possibly hostile) interfaces.** A single binary fuses two of the most-attacked surfaces on a server — SMTP/IMAP (25/465/587/143/993) and HTTP/443 — into one address space that *also* holds the TLS private keys. A fault or compromise in either path takes down both and exposes everything in the process.

**Why not just revert to postfix + dovecot + nginx + fpm?** These are trusted and robust, and were reconsidered. They are rejected as the path **not** because `maild`+`webd` deploy more simply (they don't, under the conditions above), but because the standard tools **cannot be given the deep, D-Bus-like ABP control over every aspect of daemon and GUI code that is the founding purpose of Cosmix.** That control surface is unachievable by wrapping the standard tools. So `maild` + `webd` + `disp-*` go forward — now as fault-isolated processes with explicit trusted/untrusted execution tiers, rather than as one binary.

---

## 3. What the codebase actually shows (grounding facts)

These drove the conclusions and are verifiable in the tree (`$COSMIX`, `$COSMIX`, `$COSMIX` as sibling checkouts). **Codex cold-checked all 14 against source 2026-06-04: 13 CONFIRMED, 2 corrected here** (builtin count is ~157 not 140; `umask` is test-only). None that drove the decision was refuted.

**`cosmix-webd` (v0.2.2, ~18.5k LoC)**
- Already carries the Caddy-like cert layer: a **~5,685-line ACME provisioner**, HTTP-01 solver, **hot-swappable rustls `ServerConfig` via `arc-swap`**, atomic on-disk cert state.
- Per-vhost routing (`host_router`), per-vhost `www_dir`, SQLite-backed CMS API (axum), unconditional ABP citizen, MX resolution for mail-client autoconfig.
- **Does NOT yet embed Mix.**

**`cosmix-maild` (~61k LoC incl. tests)**
- **Already embeds Mix** for inbound filtering — the working precedent for the pattern: `spawn_blocking` → build a `new_current_thread` Tokio runtime → `Evaluator::with_output(...)` → `set_global("FROM"/"TO"/...)` → `eval.execute(&stmts).await`; return value falls back to the stdout buffer. (It re-lexes/re-parses per message — fine at mail volume, not at HTTP RPS.)
- Owns SMTP/IMAP + JMAP (RFC 8620/8621), its own rustls, mailstore via `cosmix-mds`.

**`cosmix-lib-mix` (~25k LoC)**
- **`Evaluator` is `!Send` by design** (`Rc<RefCell<EvaluatorGlobals>>`, handlers as `Rc<dyn …>`). Cannot be made `Send` without a rewrite. `execute` is async. The expensive object is the *Evaluator*, not the AST — `execute` takes `&[Stmt]`, so **"cached AST, fresh evaluator per request" is the correct embedding shape** and the types support it.
- **Capability seams already exist for special forms:** `sh` is gated by `Option<Rc<dyn ShellHandler>>`, `send`/ABP by `Option<Rc<dyn AmpHandler>>`, serve-mode by `Option<Rc<dyn ServeRuntime>>` — all `None` by default, injected per-evaluator.
- **The builtin table has NO capability seam.** `call_builtin(name: &str, args: Vec<Value>)` is a **free function** dispatching ~**157 builtins**, including ungated `read_file`, `write_file`, `append_file`, `http_get`, `http_post`, `ssh_run`. The evaluator consults a fast `is_builtin()` gate *before* `call_builtin` — **that dispatch site is where a per-evaluator capability/allowlist policy should be inserted** (cheap; avoids touching 140 arms or their tests).
- **Interrupt flag exists:** `interrupted: Arc<AtomicBool>` polled at expression boundaries. A watchdog (the `Arc` is `Send`) can arm it on a deadline and unwind a runaway **pure-Mix** loop. It does **not** interrupt a blocking builtin mid-syscall (hung `http_get`/`ssh_run`/`sh`).
- **No general recursion-depth limit.** `dispatch_depth` only bounds ABP invocation nesting, not ordinary recursive Mix functions → unbounded recursion overflows the **native** stack → **process-fatal**.
- **`unsafe` is present in the interpreter** (raw-pointer deref in the numeric fast-path; `libc::kill` in builtins — `umask` is test-only).
- **`mix --serve <script>` exists:** runs a script as a **long-lived, supervised ABP citizen** (`MixServeHandler: AmpHandler`), name fixed at launch, anonymous serve forbidden. The **SPEC 18 §3.4 handler boundary already isolates a panic per dispatch** inside it.
- **A process pool / worker manager / socket-FPM front does NOT exist.** (The only "pool" in the crate is internal scope-frame recycling.) **This is the net-new work.**

**Memory-safety reality of the graph:** `webd` links **C SQLite (`rusqlite` bundled)** and **asm crypto (`ring`)**. "Safe Rust doesn't segfault" is false for these binaries — a fault in linked C or in `unsafe` is reachable and process-fatal.

---

## 4. The isolation argument (the core reasoning)

The isolation we need is a property of **address-space separation**, which is a **process** property, not a thread property. A "workers model" isolates **only if the workers are processes** — nginx masters fork worker *processes*; php-fpm is a pool of worker *processes*. Threads do not isolate, against three failure modes:

- **Panic** — *threads contain this.* The workspace builds with default `unwind` (no `panic = "abort"`), so a panic in a Mix thread unwinds that thread; `spawn_blocking` surfaces it as a `JoinError`. Mix panics are already request-local.
- **Fatal fault** (segfault, SIGBUS, stack overflow, `abort()`, OOM-kill, illegal instruction) — *threads contain nothing.* Any thread taking a real memory fault takes the whole process with it; no thread-level recovery exists. Reachable here via unbounded Mix recursion, the interpreter's `unsafe`, and linked C.
- **Compromise** — *threads contain nothing, and this settles it.* Code execution in any thread yields the entire process address space: `webd`'s rustls `ServerConfig` holds the **TLS private keys** in memory; every other tenant's in-flight data is on the same heap; every fd is usable. **Capability-gating the builtin table does not help**, because the gate runs in the address space the attacker now controls — they read the heap, they don't call the builtins.

**Supervisor:** systemd is the mature master/supervisor. Two sandboxed units give isolation between mail and web for free via declarative `SystemCallFilter=` (seccomp), `CapabilityBoundingSet=`, `PrivateTmp=`, `ProtectSystem=strict`, cgroup CPU/memory caps, `DynamicUser=`, network namespaces — more battle-tested than a hand-rolled fork-and-sandbox master. This reinforces the two-daemon split.

**Empirical backstop (mail domain):** Postfix, qmail, and OpenSMTPD are deliberately built as many small cooperating *processes* with separated privileges over local sockets, precisely because a compromise cannot be contained without separate address spaces and minimal per-component privilege. They settled this decades ago.

---

## 5. Target architecture

- **`maild` and `webd` are two sandboxed systemd units.** Independent restart, independent blast radius, independent privilege sets.
- **`webd` is the 443 front door + ACME/cert authority + static + Mix-CMS.** `maild` owns SMTP (25/465/587) and IMAP (143/993).
- **JMAP is the one real port coupling** (RFC 8620 is HTTP, wants 443). JMAP *logic* must stay in `maild` (it needs direct mailstore access). Resolve it **without merging**: `webd` terminates 443 with the cert and **proxies JMAP paths to `maild`'s local JMAP listener over a unix socket**. (This mirrors how a reverse-proxy-in-front-of-a-mail-server deploys today — to be confirmed vs. `maild` binding 443 directly; see Open Questions.)
- **Phase 1.5 — extract the cert/identity layer into a shared lib BEFORE any consolidation.** One ACME **issuer** (`webd`'s provisioner) writing a cert store that `maild` also **reads**, plus a reload signal. This delivers the entire "shared certs" payload of the abandoned merge while keeping the processes separate. It is independent of the Mix work and can proceed in parallel.
- **`disp-*` surfaces continue unchanged.** They are part of the ABP-control rationale, not affected by this decision.

---

## 6. Mix execution — two modes

**Embedded (trusted, in-host, fast).**
For operator/self-authored scripts — `webd`'s own CMS templates, `maild`'s operator-configured sieve. In-process `spawn_blocking` + fresh evaluator over a **cached AST**, with a **recursion cap** (see §7). No socket hop. This is the existing maild pattern, generalized to `webd`.

**Pooled-worker FPM-style (untrusted, out-of-process, sandboxed).**
For customer-uploaded scripts. A pool of worker processes, each **stateless per request** (fresh evaluator, cached AST), recyclable, behind a **per-pool capability/sandbox profile**.
- **Reuses** what already exists: `mix --serve`'s ABP dispatch, the §3.4 panic boundary, and the per-evaluator capability seam (configured once per pool at the `is_builtin` dispatch site).
- **Transport is likely ABP over a local unix socket** — *not* a resurrected FastCGI. The serve handler is already an `AmpHandler`. The lift is binding ABP to a local socket vs. the mesh ws hub, not a new protocol.
- **Net-new:** the worker/pool **manager** (prefork, sandbox, setuid, health, respawn, graceful reload).

**Hard rules for the split:**
- **Trust level of the script — not the daemon — picks the mode.** The same daemon uses both: trusted script → embedded; customer-uploaded script → pooled worker.
- **FPM augments embedding; it does not replace it.** Do not route trusted scripts (e.g. `maild` operator sieve) through the worker tier — it adds a serialization hop to the per-message delivery path at ~1000-customer scale for zero security gain (you don't sandbox code against itself).
- **Per-consumer / per-trust pools — NOT one global shared pool.** A shared pool would grant the *union* of all consumers' capabilities, re-merging the boundaries the split exists to create. Runtime + protocol shared (one `mix-worker` binary, one ABP contract); pools spawned per capability profile (tenant-A's pool can't see the cert store or reach the network; sieve's pool can `fileinto` a mailbox but can't open a socket).
- **Keep the FPM execution stateless** even though it borrows serve-mode's transport. Serve-mode-the-citizen is resident and stateful by design (right for something like the statecache reference citizen); an FPM worker that inherits resident state reintroduces tenant-to-tenant leakage between recycles.

---

## 7. Implementation gotchas

- **Never `fork()` from inside the Tokio daemon.** `fork()` in a multithreaded process carries over only the forking thread; a mutex held by another thread at fork time is frozen forever in the child and the inherited runtime is broken. The manager must **prefork workers that immediately `execve` a clean `mix-worker` binary** (fresh address space, no inherited runtime), keep them warm, and dispatch over the socket. No fork-per-request; no fork-without-exec.
- **The AST/opcode cache lives IN the worker** (the opcache analogue) — you cannot ship a `&[Stmt]` across a process boundary. The host sends a **script reference** (path + mtime or content hash); the worker parses on miss, reuses on hit. Per-worker caches for v1; shared-memory opcache across a pool is a v2 optimization.
- **Add a general recursion-depth limit to the evaluator** so runaway recursion returns a Mix error instead of overflowing the native stack. Needed **even for the trusted in-process path** (it's the residual fatal-fault risk there). `dispatch_depth` shows the pattern; this is a *separate* counter for ordinary call recursion.
- **Keep the management/control-plane API native (axum).** Do not route Cosmix's own control plane through the tenant Mix sandbox.

---

## 8. Open questions for review

- **`webd.routes` schema** — declared route table `(host, path-pattern, method) → handler{ mix-ref | static-dir | native }`. Store in the props substrate (`vhost_directory` / `vhosts_namespace` already model per-vhost config there); make it reloadable.
- **Non-JSON response typing from Mix handlers** — proposed `Value` contract: `String → 200 text/html`; `Value::Object { status, headers, body } → typed`. The json/serde infra is already in the crate.
- **Tenant-bound ABP scoping** — default-off; per-request, inject a namespace-scoped `AmpHandler` or `None` when building the evaluator.
- **Untrusted workers MUST be denied `noded`-broker reach — security-load-bearing, not defence-in-depth (SPEC 13 §9a, 0.4.12).** A pooled (untrusted) worker that reaches the local `noded` broker's loopback listener would be admitted **ungated**: D2 broker admission gates only the *inter-node* boundary and does **not** distinguish trusted-vs-untrusted *same-node* processes — once a local session opens, it is local-trust. So an untrusted worker reaching that socket could register or impersonate a local service (the *intra-node* analogue of the §9a B4 residual). The tenant-bound ABP scoping above (`AmpHandler = None`) is the in-process hook, but it MUST be backed by the **pool seccomp/network profile blocking the `noded` socket explicitly** — confirm the per-pool profile denies it, so capability gating and the kernel sandbox agree. (The broker deliberately trusts same-node origin; keeping untrusted code off that socket is *this layer's* job.)
- **443 JMAP routing** — confirm `webd`-as-front-door proxying to `maild`'s local JMAP listener vs. `maild` binding 443 directly.
- **Worker protocol, concretely** — confirm ABP-over-local-unix-socket as transport vs. a minimal length-prefixed frame; define the request frame (script-ref, capability/tenant id, headers, body) and response frame.
- **Pool policy** — sizing and recycling (`pm.max_requests` analogue), per-pool uid + seccomp profiles, cold-spawn vs. warm-pool sizing.
- **Shared-memory opcache** across a pool (v2).

---

## 9. Decided against — do not re-litigate

- **Single combined `cosmix-vhostd` binary** (maild + webd + Mix in one process) — rejected for fault and compromise isolation between two internet-facing attack surfaces that would share an address space holding the TLS private keys.
- **In-process threads as the multi-tenant isolation boundary** — rejected; threads contain panics but not fatal faults or compromise (shared address space).
- **Reverting to postfix + dovecot + nginx + fpm** — rejected; cannot deliver the ABP control surface that is Cosmix's reason to exist.
