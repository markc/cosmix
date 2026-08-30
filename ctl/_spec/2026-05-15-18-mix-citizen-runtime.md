---
title: Mix Citizen Runtime — Mix Scripts as First-Class ABP Daemons
chapter: 18
version: 0.1.3-draft
status: draft
date: 2026-05-28
companion: 2026-05-09-10-cosmix-daemon-identity.md
---

# Mix Citizen Runtime

> **Status: v0.1.3 DRAFT 2026-05-28.** Design of record per a
> direction-setting decision (commit to "Mix daemons as a first-class
> citizen class"). §10 partially resolved: the chapter number (§10.1),
> the yield/reentrancy direction & mechanism (§10.3 — explicit
> `on <cmd> async` modifier, Phase 2 shipped 2026-05-28), and the
> SPEC-10 citizen sub-range (§10.2, resolved by SPEC-10 v1.2.0) are
> **decided**; Phase 1 (§9) is greenlit; Phase 2 (§9) is **shipped**.
> v0.1.3 records the Phase 2 landing: §10.3 closed (explicit
> mechanism + per-`send` `timeout=<sec>`), §3.7 normative-dependency
> paragraph reframed from "open mechanism sub-question" to "decided &
> shipped", §9 Phase 2 marked SHIPPED with acceptance harness
> reference. v0.1.2 (2026-05-16) was the first cold-review revision
> over v0.1.1: structural (not behavioural) Class S/C discriminator
> (§3.7/§4), reconnect re-subscribe contract generalised to all
> `subscribe()` calls (§3.3), §3.7 deployment matrix, §0
> dependency-shape split, plus Q1 (route/authz artifact — §10.4) and
> Q2 (cycle-detection honesty — §3.7) surfaced. This chapter is
> normative for the *contract*; the implementation is phased (§9)
> and its open engineering dependencies are stated as explicit
> phase-gates, not hand-waved — §3.3 reconnect, §3.7 cooperative-
> loop, and the cross-cutting §7 principal/origin identity model
> (the last not authored here; §10.6).

## §0 Purpose & scope

This chapter specifies a runtime contract under which a **Mix script
(Ch04) runs as a first-class, supervised ABP citizen**, in either of
two modes (§1, §3): a **registered serve-mode citizen** — a named,
addressable, restartable broker service with its own SPEC-10 identity,
long-lived (resident) or an ephemeral that opts to register; or an
**unregistered transient citizen** — the §2 cheap-many path: a
short-lived oneshot script run under its supervisor's SPEC-10
identity, with no service name and no addressability. The named/
addressable/SPEC-10-slot contract below is the *registered* mode's;
the unregistered mode is a supervised script run, deliberately
cheaper and deliberately less.

It also specifies **webd's role as the thin HTTP/WebSocket ↔ ABP
boundary** (§7): the single mediated surface through which a
*registered* Mix citizen MAY be reached from outside its broker,
including the inter-mesh / external world. (An unregistered transient
citizen has no service name and is not addressable at all — webd has
nothing to route to it; only registered citizens are externally
exposable, and then only via an explicit §7 route.)

The thesis (CLAUDE.md mandate, ARexx clause): *every application
becoming agent-addressable is the same property as every application
becoming script-addressable.* Making a supervised mesh service
authorable as a single legible Mix file — rather than a Rust crate
requiring `cargo build` + fleet redeploy — is the modifiability and
reconstructibility criteria in their strongest form. This chapter is
the contract that makes that safe and bounded.

**In scope:** the serve-mode lifecycle contract; reconnect/supervision
semantics; the concurrency-safety classification; citizen↔citizen
orchestration; MCP/agent orchestration boundary; the trust model; the
webd boundary; UI-citizen interaction.

**Explicitly out of scope / non-goals:**

- This is **not a sandbox for untrusted code** (§6). A Mix citizen is
  a *trusted, full-capability* process, same trust class as any Rust
  daemon.
- This does **not** itself author changes to the ABP wire format
  (Ch01), the Mix grammar (Ch04), or the display vocabulary (Ch05).
  It adds a runtime *mode* and a service *contract*. Its dependencies
  are of **two distinct kinds** — both must land before §9 acceptance
  is satisfied, but they differ in what closing them requires:
  - **Future amendments consumed but not authored here** (require
    *authoring* by their owning chapter): (a) the Ch04 yield-on-`send`
    amendment (§3.7, §9 Phase 2); (b) the cross-cutting Ch01 + Ch05
    §15.2 principal/origin identity model (§7, §9 Phase 3, §10.6).
    "Does not author" is the precise claim — SPEC 18 *consumes* both;
    it does not specify either.
  - **Current implementation gaps required for §9 acceptance**
    (require only *coding* against an already-settled contract — no
    amendment): (c) the §3.3 supervised reconnect/backoff/re-subscribe
    path (absent today in `cosmix-mix` / `cosmix-lib-client`,
    verified); (d) webd's missing registered-service exposure for the
    §7 boundary. These are not open design questions — the contract is
    fixed here; they are unwritten code.
- This does **not** redefine daemon identity. Identity and OS
  lifecycle are SPEC-10's; this chapter consumes them (§2) and must
  not introduce a parallel mechanism.

## §1 Concepts & vocabulary

Terms below extend the Ch00 glossary (`process`, `service`, `broker`,
`mesh`, `node`); they do not redefine it.

| Term | Definition |
|------|-----------|
| **Mix citizen** | A Mix script (Ch04) run as a participant of its node's ABP mesh, in **one of two execution modes**: (a) a **registered serve-mode citizen** — resident, or an ephemeral that opts to register — governed by the §3 serve-mode contract, a broker service with a SPEC-10 identity; (b) an **unregistered transient citizen** — the §2 cheap-many path: a plain oneshot script run under a supervisor's identity, *not* `--serve` and *not* §3-governed, no service name. "Citizen" names the class; only mode (a) is a broker service and owes §3.6/Ch07 conformance. |
| **Serve mode** | The `mix --serve` execution mode (§3): the **registered** mode — run init once, then stay resident dispatching inbound ABP into `on` handlers under supervision. The unregistered transient path is not serve mode. |
| **Resident citizen** | A long-lived Mix citizen supervised by systemd, registered as a SPEC-10 service with its own UID/GID (§2). |
| **Ephemeral citizen** | A short-lived Mix citizen run on-demand/once (cron-driven or oneshot). It either registers a service name (own SPEC-10 slot, addressable for its lifetime) **or** runs unregistered under its supervisor's identity (no slot); §2 makes the unregistered form the cheap-many pressure valve. Either way it keeps a full ABP client connection — **topic pub/sub (`emit`, topic `subscribe` per Ch03 §3.11.3, topic-driven `on` handlers) works in both forms** under the Ch03 §3.11.1 connection-scoped synthetic peer identity; the unregistered form forgoes only a *request-addressable service name* and the slot, **not the mesh**. The glossary does not assert it is "always registered" — §2 is authoritative. |
| **Concurrency class** | The classification (§3.7) — Class S (sequential-safe today) or Class C (requires the §3.7 loop fix) — that governs whether a citizen may be deployed before the cooperative-loop dependency lands. The discriminator is **structural** (is it a *registered* serve-mode citizen, hence concurrently-callable?), not behavioural (observed traffic): see §3.7 for the structural discriminator and the sole-caller carve-out, and §3.7's deployment matrix for earliest-deployable by shape. |
| **webd boundary** | The thin HTTP/WS↔ABP translator (§7); the *only* sanctioned external surface for a citizen. |

## §2 Identity & lifecycle — delegated to SPEC 10, not reinvented

A Mix citizen's OS identity and process lifecycle are governed
entirely by `2026-05-09-10-cosmix-daemon-identity.md`. This chapter adds **no**
new identity mechanism. Concretely:

- **Identity is assigned by systemd, never by the `mix` binary.** Per
  SPEC 10, an identity is received via a `sysusers.d` `u <name> <uid>`
  entry plus a systemd unit with `User=`/`Group=`; the `mix` binary
  MUST NOT `setuid`/`setgid` itself. A **registered serve-mode
  citizen** specifically takes a SPEC-10 **citizen-identity entry**
  (v1.2.0): a `u` line in the dedicated **600–699 citizen band**
  governed by **scoped, gated reuse** (R7/R8 — mesh-wide automated
  purge-verification AND a 30-day quarantine), *not* a strict-no-reuse
  500–599 daemon-identity entry. (Rust daemons and the broker itself
  remain 500–599 append-only daemon-identity entries — the bands are
  disjoint; see §10.2.) A resident Mix citizen is thus a SPEC-10
  daemon-class process whose unit's `ExecStart` is
  `/opt/cosmix/bin/mix --serve /usr/local/lib/cosmix/<name>.mix`.
- **Every resident (registered) citizen is a normal SPEC-10 daemon.**
  Per SPEC-10 (canonical registry; one entry, one unique ABP service
  name, one UID/GID, one daemon leaf per service) there is **no
  shared-identity registered citizen** and this chapter introduces no
  parallel mechanism (per §0). A resident Mix citizen consumes one
  SPEC-10 registry slot with the same *structural* shape as a Rust
  daemon (one entry, one name, one UID/GID, one leaf) — a deliberate,
  bounded cost, not a deficiency. It differs from a Rust daemon in
  *reclamation policy only*: a Rust daemon is a strict-no-reuse
  500–599 daemon-identity entry, whereas a registered citizen is a
  600–699 **citizen-identity** entry whose slot is **reclaimable**
  under SPEC-10 v1.2.0 R7/R8 (mesh-wide automated purge-verification
  AND a 30-day quarantine). The slot cost is therefore *recoverable*,
  not permanent — which is exactly what makes a disposable citizen
  class viable without exhausting the registry (§10.2).
- **The cheap-many case is the *ephemeral, unregistered* citizen.** A
  short-lived Mix citizen that does **not** register a service name is
  not a SPEC-10 registered service and consumes **no** registry slot;
  it runs under its supervisor's identity (e.g. `cosmix-cron`, UID
  507). This is the SPEC-10-consistent pressure valve for "many small
  citizens": only things that need to be *request-addressable* spend a
  slot. **An unregistered citizen still has a full ABP client
  connection (normative): it retains full topic pub/sub** — `emit`,
  topic `subscribe` (Ch03 §3.11.3), and topic-driven `on` handlers —
  under the Ch03 §3.11.1 *connection-scoped synthetic peer* identity,
  no service name and no slot required. The only Ch03 capability it
  lacks is anonymous-*publisher* snapshot/back-off continuity across a
  reconnect (Ch03 §3.11.1: a disconnected anonymous connection gets a
  fresh identity on reconnect, so the broker treats it as an unrelated
  publisher and its prior snapshot context is lost); subscribe /
  `emit` / topic `on` are fully supported. "Unregistered"
  therefore means *not request-addressable*, **not** *off the mesh* —
  the common inference "no service name ⇒ cannot do pub/sub" is wrong
  and this paragraph is its normative refutation. Whether SPEC-10
  should carve a dedicated citizen sub-range was a SPEC-10 amendment
  question, **resolved by SPEC-10 v1.2.0**: it adds a *citizen-identity
  entry class* in a dedicated 600–699 band with scoped, gated reuse
  (SPEC-10 §2.2/§2.5, R7/R8 — mesh-wide automated purge-verification
  AND a 30-day quarantine), disjoint from the 500–599 daemon/shared
  window. A **registered serve-mode citizen is a SPEC-10
  citizen-identity entry** (still one slot, one unique ABP name, one
  daemon leaf — but reclaimable under R7/R8, unlike a strict-no-reuse
  daemon slot). See §10.2 for the resolution record.
- **OS lifecycle is systemd's.** Restart policy, start ordering
  (`After=cosmix-noded.service`), resource limits, and journald
  capture are unit-file concerns (template in §9 Phase 1), identical
  in shape to the existing `cosmix-{noded,maild,webd}.service` units.
  The serve-mode contract (§3) defines only the *in-process*
  behaviour systemd cannot provide (registration, reconnect, handler
  isolation, graceful dereg).
- **Ephemeral citizens** are supervised by `cosmix-cron` (SPEC-10 UID
  507; chapter planned) once it exists; until then by a
  `Type=oneshot` unit or direct invocation. Children a citizen spawns
  must follow `feedback_pre_exec_pdeathsig` (PR_SET_PDEATHSIG +
  getppid race-check) so a citizen crash does not orphan them.

## §3 The serve-mode contract

`mix --serve <script.mix> [--name <service>]` is the normative entry
point. The primitives largely exist in `cosmix-mix` today
(`amp.rs::register_as` → `cosmix-lib-client::register_as`;
`run_event_pump()` already invoked post-init in `main.rs`); this
section specifies the *robustness contract* around them. Each
subsection states a MUST-level requirement.

**Scope of this section.** §3 governs the **registered** serve-mode
citizen — every resident citizen, plus any ephemeral citizen that
opts to register a name (§2). `--serve` *is* the registering mode and
therefore requires a name; that is why anonymous `--serve` is an
error (§3.1). The **unregistered ephemeral** path of §2 is **not**
`--serve` and is **not** governed by this section: it is a plain
transient script run (`mix <script>` / a `Type=oneshot` invocation)
under a supervisor's identity, calls no `register_as`, exposes no
*request-addressable service name*, and owes no §3.6/Ch07 service
conformance because it is not a broker service. (It still holds a
full ABP client connection with full topic pub/sub per §2 — "no
service name" is **not** "off the mesh"; it simply has no registered
name for request routing or §3.6 conformance to attach to.) There is thus no contradiction between "§3
requires a name" and "§2 has an unregistered case" — they are two
distinct execution modes, and only the registered one is a *serve-mode
citizen*.

### §3.1 Registration

On start, the citizen connects to its local broker and calls
`register_as(<service>)`. The service name comes from `--name`,
defaulting to the citizen's SPEC-10 **ABP service name** — the `<d>`
token *without* the `cosmix-` prefix (SPEC-10 §2: the POSIX/systemd
name is `cosmix-<d>` and matches `^cosmix-[a-z][a-z0-9-]{1,30}$`; the
ABP service name is the same `<d>` token minus the prefix). The
default MUST be the ABP form, never the POSIX `cosmix-<d>` form
(a `cosmix-*` ABP name is invalid by SPEC-10 §2). It is a registered
broker name governed by Ch01 §9.1 (Service Registration) with the
ABP-name derivation pinned by SPEC-10 §2 R6 (the registered name MAY
differ from the binary, e.g. `cosmix-disp-skia` → `disp-skia`). Mix
does **not** have script frontmatter; no `service:` header is defined
or used (introducing one would be a Ch04 change, out of scope here).
The *commands* the citizen answers MUST obey Ch02 command-vocabulary
namespace discipline. A serve-mode citizen that fails to obtain a
name, or fails the initial registration after the §3.3 backoff
budget, MUST exit non-zero (systemd restarts it); anonymous serve
mode is an error.

### §3.2 Init then pump

The script's top-level body executes exactly once as initialisation
(open resources, subscribe to topics, register `on` handlers). The
runtime then enters the event pump and dispatches each inbound ABP
message to its matching Ch04 `on` handler — Ch04 §"Event handlers
(`on`)" owns the grammar (`on event.name … end`, unquoted, statement
position); this chapter does not restate or alter it. Handlers
serialize one-at-a-time per Ch05 §7.3 (see §3.7). This reuses the
existing `run_event_pump()`; serve mode makes it the supervised,
non-terminating path rather than a post-script tail.

### §3.3 Reconnect & backoff — PHASE-GATE (the gating defect)

A citizen that does not survive a broker (`cosmix-noded`) restart is
not a daemon. **Today there is no reconnect/backoff path anywhere in
`cosmix-mix` or `cosmix-lib-client::native`** (verified: zero
`reconnect`/`backoff`/`retry` occurrences). This is the single
gating defect for "long-running."

Normative requirement: the runtime MUST implement a supervised
`connect → register → pump` loop such that, on transport loss, it
re-enters connect with **bounded exponential backoff with jitter**,
re-registers the service name, and resumes the pump. While
disconnected the citizen is in an explicit `disconnected` state:
inbound dispatch is impossible and any outbound `send` MUST fail fast
with a typed error — **no outbound queue, bounded or unbounded** (any
queue — even a bounded one — is the
`feedback_change_stream_fanout_atomicity` / partial-truth failure
shape: it hides the outage behind a buffer instead of surfacing it.
The contract is *surface the disconnected state to the caller via a
typed error*, never absorb the write).

**Registration provenance (normative, version-discovery contract).** At
`register` the citizen SHOULD send its build provenance — a
`RegisterProvenance` body (binary/version/git_sha/git_dirty/build_time/pid/
started_at; SPEC 02 §4.1) — so its build is discoverable fleet-wide via
`noded.list`. Build it ONCE at process start and re-send it on every
`register` (so `started_at` is the true process start and survives a §3.3
reconnect, rather than re-stamping to the reconnect time). Omitting it is
permitted (the broker registers name-only); the provenance is the cheapest
legibility signal a fleet of agents has for "what build runs where," so a
resident serve-mode citizen SHOULD supply it. The normative schema is
SPEC 02 §4.1; reference clients live in `cosmix-lib-client/src/native.rs` and
`cosmix-lib-client/src/supervised.rs`.

**Conformance, not free inheritance (normative).** The reference
implementation of this loop MUST live in `cosmix-lib-client` so the
common path is correct by default. But a shared library does not by
itself close the gap fleet-wide — a daemon that hand-rolls its broker
connection bypasses it (this is precisely the
`feedback_refactor_silent_noop_audit` partial-truth shape: a fix that
*looks* universal while a bypass quietly preserves the old
behaviour). Therefore the requirement is stated as conformance:
**every ABP-registering daemon (Mix citizen or Rust) MUST either use
the shared reconnecting client or independently satisfy this §3.3
contract**, and §6/§9 acceptance verifies the citizen's *observed*
reconnect behaviour, not its dependency list.

On reconnect the runtime MUST re-establish the citizen's broker-side
state, not just the socket: re-`register_as` the service name, and
**re-subscribe every topic the citizen has subscribed to via any
`subscribe()` call — init-body *or* handler-body, the full set, not
only the init-body subset** (Ch03). Subscription is broker-side state
from the citizen's point of view (the broker keeps zero
client-recoverable subscription state across a transport drop), so
tracking the citizen's subscription set and replaying *all* of it on
reconnect is categorically the runtime's job — exactly as
re-`register_as` is — *not* the citizen author's. Restricting replay
to init-body subscriptions would silently drop any dynamically-added
subscription on the next bounce: the same partial-truth /
silent-queue failure shape this section exists to forbid, merely
relocated. **Normative implementation obligation (pins §9 Phase 1
Step 4):** the supervised client's subscription registry MUST record
every `subscribe()` call as it happens (the recording is what makes
replay possible — the broker exposes nothing to enumerate), and the
reconnect path MUST replay the entire registry. Broker-side
registration and subscriptions do not survive a transport drop and
are not silently assumed to. State *derived* from the stream (caches,
in-flight orchestration) is the citizen's responsibility to
rehydrate; the runtime MUST surface a reconnect event/log so the
citizen can do so deterministically rather than serving stale derived
state as if fresh.

### §3.4 Handler fault isolation

A panic, `die`, or uncaught error inside one `on` handler MUST NOT
terminate the citizen. The runtime MUST: catch it at the per-request
boundary, log it (structured, §3.6), emit an error reply to the
caller if the inbound message was a request expecting one, and
continue the pump. The per-request fault domain is a contract, not
best-effort: one malformed request must not deny service to all
others.

### §3.5 Graceful shutdown

On `SIGTERM` (wired to `cosmix-lib-daemon::shutdown_signal()`), the
citizen MUST: stop accepting new requests, allow in-flight handlers a
bounded grace period to complete, **deregister** its service name
from the broker, then exit 0. An ungraceful exit (grace exceeded)
exits non-zero after best-effort deregister so the broker's registry
does not retain a dead name. The Ch02 `QUIT` universal (Ch02 §3:
"clean up, flush state, disconnect") MUST invoke this *same* graceful
path — `QUIT` and `SIGTERM` are two entry points to one shutdown
sequence, not two behaviours.

### §3.6 Readiness, health, observability

- The runtime emits structured logs via the
  `cosmix-lib-daemon::init_tracing` convention (same log dir as all
  daemons) — never ad-hoc stdout in serve mode.
- A **registered serve-mode citizen** (the §3 scope — every resident
  citizen; not the §2 unregistered-ephemeral path, which is no broker
  service and owes none of this) is a broker service and therefore
  owes the **full Ch07 daemon conformance**, identical to a Rust
  daemon: the Ch02/Ch07-L0 universals `HELP`, `INFO`, **and `QUIT`**
  (Ch07 §9 defines L0 as `HELP`/`INFO`/`QUIT`; `QUIT` maps to the
  §3.5 graceful-shutdown path — it is not optional and not a no-op),
  plus the Ch07 property surface `<svc>.props.get` / `.list` /
  `.describe` (and `.watch` / `<svc>.props.changed` at L2+), exposing
  lifecycle state (uptime, start time, operating mode, health) per
  Ch07 §"Lifecycle state".
  There is no separate one-off "health command" — health is a
  property in that tree. This is what makes a registered Mix citizen
  exactly as legible as a Rust one — the §0 legibility criterion.
- The runtime SHOULD `sd_notify(READY=1)` after the first successful
  registration so units may use `Type=notify`; absent that,
  `Type=exec` is the documented fallback.

### §3.7 Concurrency model — PHASE-GATE (the cooperative loop)

Mix `on` handlers run in a **cooperative single-threaded event loop**
(ABP Display Protocol §7.3; `project_mix_yield_question`). A
synchronous `send` to a slow service inside a handler stalls the
entire queue — for a UI this is "feels frozen"; for a request-serving
citizen it serialises all concurrent requests behind the slowest
downstream call. This is stated plainly because it bounds *which
citizens are safe to deploy when*, and it MUST NOT be hand-waved.

Citizens are classified:

The discriminator is **"is this citizen a shared service whose
handler blocks while other callers wait?"** — *not* "does it issue
slow `send`s." Crucially, the property that answers it is
**structural, not behavioural**: it is *registration*, not observed
traffic. A **sole-caller** orchestration driver is a Mix script run
as a *transient ABP client* (cron/oneshot/CLI invocation) — i.e. **not
a registered serve-mode citizen at all**. Because it has no service
name it is, by §2's definition, not request-addressable, so it has
*no concurrent callers it could head-of-line-block*; its cooperative
loop serialising its own sequential `send`s (the §4 pipeline /
scatter-gather / review-loop driver) is the *correct* behaviour, not
a defect. A **registered serve-mode citizen**, by contrast, is
*structurally* concurrently-callable the moment it has a name —
regardless of how many callers it has *today* — and is therefore
classified by the slow-`send`-head-of-line-blocking test **alone**.
The sole-caller carve-out does not apply to it: "I have one client
right now" is a runtime accident, not a structural exemption, and
MUST NOT be read as one (doing so would back-door a Class C citizen
into Phase 1). Class C is triggered by *being a registered,
shared-addressable service whose handler head-of-line-blocks
concurrent callers* — not avoided by *driving an orchestration*, and
not conferred by *current* call multiplicity.

- **Class S (sequential-safe).** A registered serve-mode citizen with
  low request rate, fast handlers, and handlers whose `send`s target
  only fast local services; **or** an orchestration driver that is
  *structurally* its own sole caller — i.e. a transient ABP client
  (cron/oneshot/CLI invocation), **not a registered serve-mode
  citizen** (no service name ⇒ no concurrent callers ⇒ its loop
  serialises only its own sequential work). A *registered* driver is
  **not** Class S by virtue of being a driver — being registered makes
  it structurally concurrently-callable, so it is classified by the
  discriminator above like any other shared service (its slow
  sequential `send`s would head-of-line-block its concurrent callers,
  making it Class C). Correct under the current loop. **Deployable in
  Phase 1.**
- **Class C (concurrency-required).** A **registered,
  request-addressable citizen** (structurally concurrently-callable
  by virtue of having a service name) where a handler blocks on a
  slow/remote downstream — a request-*serving* service (incl. a UI
  event server, §8, and a *registered* driver), the only excluded
  shape being a structurally sole-caller driver (an *unregistered*
  transient client, no service name — §3.7 carve-out). **NOT
  deployable until the loop fix lands (Phase 2).** The highest-value Class C
  citizen — a shared request server taking *external* traffic —
  additionally gates on §7/Phase 3: Phase 2's yield-on-`send` makes
  an *intra-mesh* acyclic Class C citizen deployable, but an
  externally-exposed one is not deployable until the §7 webd boundary
  and the cross-cutting principal/origin field (§7, §10.6) also land.
  Class C therefore
  does not arrive in bulk at Phase 2 the way an earlier reading might
  imply; only the intra-mesh acyclic subset does.

Normative dependency: Class C requires a Ch04 Mix-language amendment
making the event loop non-blocking across `send`. **Decided & shipped
(2026-05-28): yield-on-`send` without handler reentrancy via an
explicit `on <cmd> async` modifier** — Class S (no modifier)
preserves the run-to-completion atomicity every *already-shipped*
Mix script (`sysmon.mix`, the CMM scheduler scripts) was authored
under; Class C (modifier) yields the dispatch reader at every
`send`/`reply`/`sleep_ms` await point so concurrent acyclic
invocations interleave. A handler is never re-entered while
suspended; synchronous request cycles stay prohibited *by design*
(§3.7 deadlock corollary), not deferred. Co-shipped: a per-`send`
`timeout=<sec>` kwarg (cooperative cancellation; pending-reply slot
freed on timeout). The previously-open implicit-vs-explicit
mechanism sub-question is **closed** (§10.3) — the burden was on
implicit and it didn't carry; the explicit form is additive surface,
opt-in, and test-catchable at author time. Normative landing site:
Ch04 §"Event handlers (`on`)" + Ch05 §7.3 rules 6–7. Phase 2's win
is bounded: yield-on-`send` removes head-of-line blocking for
**acyclic** slow-downstream handlers only.

**Deadlock corollary — and what Phase 2 does *not* do.** A
synchronous request cycle between single-threaded citizens (A→B→A)
deadlocks today. Ch05 §7.3 *intentionally* forbids handler
reentrancy ("a handler that blocks … holds the event loop … this is
intentional and prevents reentrancy bugs"). Yield-on-`send` does
**not** by itself make a reentrant cycle safe — it would expose
exactly the reentrancy bug class Ch05 §7.3 deliberately precludes.
Legalising any synchronous cycle would require a foundational Ch05
§7.3 + Ch04 reentrancy amendment defining continuation/handler-state
semantics and proving them safe — **and no such amendment is
planned** (§10.3: synchronous cycles are prohibited *by design*, not
deferred). Synchronous cycles are therefore PROHIBITED at every phase
(§4) **permanently by design**, not "deferred to Phase 2" and not
"pending a future amendment that would relax this".

**Enforcement is contractual, not detected (honest scope, Q2).** The
prohibition is a *contract*, not a broker-enforced invariant: cycle
detection is **not a broker capability today**, and SPEC 18 does
**not** require the broker to add one. A citizen author who writes a
synchronous cycle anyway will deadlock **silently** — no error, no
broker rejection. "Prohibited by design" therefore means *the
contract forbids it and the safety mechanism is the contract*, not
*the broker stops it*. This is stated explicitly so no reader infers
broker enforcement that does not exist; whether the broker should
eventually gain cycle detection is a separate Ch01/Ch05 broker-
capability question, out of scope here.

**Deployment matrix (§3.7 + §9, presentation of decisions already
made above).** Earliest phase at which each citizen *shape* is
deployable — the load-bearing answer to "when can I ship citizen X":

| Citizen shape | Earliest deployable |
|---|---|
| Class S, intra-mesh | **Phase 1** |
| Class C, intra-mesh, acyclic | **Phase 2** (Ch04 yield-on-`send`) |
| Class C, external-facing, acyclic | **Phase 3** (Phase 2 **+** §7 webd boundary **+** cross-cutting principal/origin field, §10.6) |
| Synchronous request cycles (any phase) | **Prohibited permanently by design** (§3.7 deadlock corollary; §10.3) — never deployable |

The matrix introduces no new rule; it makes the §3.7/§9 gating
visible at the resolution a deploy decision needs. Note the
highest-value shape (a shared *external* request server — the
autoconfig-class surface that motivated this arc) is gated on Phase 3,
**not** Phase 2 alone.

## §4 Orchestration — citizen ↔ citizen

A Mix citizen may `send`/`address` other citizens, making
multi-citizen orchestration native. This is the project's own
dual-reviewer loop (Claude proposes, Codex critiques) expressed as
mesh citizens — and `project_agent_runtime_unification` expressed
natively.

**A sole-caller orchestration driver is Class S (§3.7), not Class
C.** A review-loop / pipeline / scatter-gather driver that is
*structurally* its own only caller — a transient ABP client
(cron/oneshot/CLI invocation), **not a registered serve-mode
citizen** (no service name ⇒ no concurrent callers) — issues
sequential `send`s and awaits each: correct under the current
cooperative loop and **deployable in Phase 1** with no yield-on-`send`
dependency. (A *registered* driver is not Class S by being a driver:
registration makes it structurally concurrently-callable, so it is
classified by the §3.7 discriminator like any other shared service —
"only one caller today" is a runtime accident, not an exemption.) The
dual-reviewer loop specifically is a sole-caller Class S sequential
driver, *not* a Class C use case — the Class C concern arises only if
a *worker* citizen it calls is itself a registered shared service
whose handler blocks (§3.7), which is the worker's classification,
not the driver's.

Sanctioned patterns: **pipeline** (A→B→C, each transforms), **scatter
-gather** (driver fans a request to N workers, merges replies),
**supervisor/worker**, **review-loop** (a proposer citizen and a
critic citizen iterating). Topic fan-out uses Ch03 pub/sub, not
hand-rolled lists.

Correctness constraints (normative):

- **Synchronous cycles are PROHIBITED at every phase.** A→B→A
  synchronous request cycles MUST NOT be deployed — Ch05 §7.3
  non-reentrancy makes them deadlock, and Phase 2's yield-on-`send`
  does **not** legalise them (§3.7 deadlock corollary). Orchestration
  MUST be acyclic in the synchronous-request graph, or break the
  cycle with fire-and-forget `emit` + topic replies (Ch03). This
  constraint is **permanent by design** (§10.3: no Ch05 §7.3
  reentrancy amendment is planned; cycles are prohibited by design,
  not deferred) — it does not relax unless a future foundational spec
  explicitly overturns that decision, which this chapter neither
  authors nor anticipates.
- Orchestration handlers MUST set timeouts on downstream calls and be
  idempotent where retried; an orchestrator that blocks forever on a
  dead worker is a §3.7 head-of-line-block in disguise.
- Inbound orchestration requests are validated like any other (§6).

## §5 MCP & agent-orchestration boundary

A Mix citizen may act as an MCP client / agent-orchestration loop
body: it reaches MCP tools by calling the `cosmix-mcp` bridge (SPEC-10
UID 505), itself a mesh citizen, over ABP — or addresses other
tool-exposing citizens. This makes "a Mix citizen as the loop that
calls Claude/Codex/MCP tools and routes results" a native substrate
pattern rather than bespoke glue.

Boundary (normative): MCP/tool invocation from a citizen is
**outbound capability use under the citizen's effective SPEC-10 UID**
— its own for a registered serve-mode citizen, the supervisor's for
an unregistered transient citizen (§6) — governed by §6, not a
separate trust regime. This chapter specifies only the
boundary and trust framing; concrete tool-surface unification remains
`project_agent_runtime_unification`'s scope and MUST NOT be
re-specified here (avoid two sources of truth — the
`feedback_refactor_silent_noop_audit` partial-truth shape).

## §6 Trust & security model

A Mix citizen is a **trusted, full-capability** process: it has the
full Mix builtin surface (fs/exec/net), the same trust class as any
Rust daemon. It runs under **its own SPEC-10 UID for a registered
serve-mode citizen, or under the supervisor's SPEC-10 UID for an
unregistered transient citizen** (§2) — either way a real
SPEC-10-registered UID, never an ad-hoc one: a registered serve-mode
citizen takes a 600–699 citizen-identity UID (v1.2.0), an unregistered
transient citizen inherits its supervisor's 500–599 daemon-identity
UID (e.g. `cosmix-cron` 507). The WG /24 is the trust
domain (Ch01 §10); WireGuard membership is the credential.

Normative statements:

- **Not a sandbox.** This runtime does not, and is not intended to,
  safely execute untrusted or multi-tenant Mix. The capability on
  offer is *cheap trusted citizens authored/reviewed by the operator
  or an agent*, not *safe arbitrary code execution*. No in-process
  untrusted eval mode is specified or permitted under this chapter.
- **WG-trusted ≠ caller-benign.** Per
  `feedback_amp_wire_trust_boundary`, inbound ABP request fields MUST
  be validated at the handler before they parameterise a path, an
  exec, SQL, or an outbound call. Trust-domain membership bounds *who*
  can reach the citizen; it does not make request *content* safe.
- **Isolation via identity.** Every registered (resident) citizen has
  its own SPEC-10 UID (§2) — the fault/data isolation primitive. There
  is no shared-identity registered citizen; unregistered ephemeral
  citizens inherit their supervisor's identity and trust (§2).
- **External exposure is mediated only.** A Mix citizen MUST NOT bind
  a public port directly and MUST NOT be reachable from outside its
  broker except through an explicit webd route + authz entry (§7).
  Default-deny: a citizen is internal until a webd route is
  deliberately added.

## §7 webd as the thin HTTP/WS ↔ ABP boundary

webd is the single sanctioned surface between HTTP/WebSocket (intra-
mesh *and* inter-mesh/external) and Mix citizens. webd is **specified**
as the `webd` ABP service (SPEC-10 UID 502; Ch01 registered-service
model) — it is normatively a mesh citizen. The gap is implementation,
not specification: the *current* `cosmix-webd` binary depends only on
`cosmix-lib-config` and `cosmix-lib-daemon`, does not use
`cosmix-lib-client`, and does not register the `webd` service (it
raw-WS-proxies and HTTP-reverse-proxies instead). This chapter
requires closing that implementation gap.

- **Close the webd citizen-implementation gap.** The `cosmix-webd`
  binary MUST register as the `webd` service via `cosmix-lib-client`
  and, like any ABP-registering daemon, MUST satisfy the §3.3
  reconnect conformance — *not* "for free": conformance is verified by
  behaviour (§3.3), not by linking the crate.
- **Generic, declarative routing.** webd holds a route table mapping
  `(method, host/path-pattern) → (ABP service, command, arg-mapping)`
  and `WS upgrade → ABP topic/stream bridge`. Adding a dynamic surface
  is a route-table row + a citizen — **webd recompiles ~never**. This
  is the general capability of which the (already-settled, MX-derived)
  mail-autoconfig case is merely the first worked consumer; this
  chapter does not reopen that decision.
- **What stays in webd (the Rust trust boundary):** TLS termination,
  `Host`/header/path validation and escaping, body-size limits, rate
  limiting, response rendering, and — load-bearing — **authz**:
  which route may reach which citizen, **default-deny via an explicit
  allowlist**. An unlisted route reaches no citizen.
- **Principal/origin propagation is a cross-cutting substrate
  identity model, not a SPEC-18-local amendment (normative — route
  allowlisting is not sufficient).** Route-to-citizen allowlisting
  alone would let webd present every external request to a citizen as
  indistinguishable benign mesh traffic — the citizen could not tell
  an authenticated intra-mesh caller from an anonymous internet one.
  The naive fix — webd writes an in-band origin/principal value and
  the citizen believes it — is **unsound by contract**: webd is
  itself a local process, and Ch05 §15.2 already mandates that local
  processes MUST NOT set transport-origin metadata and the broker
  **overwrites any process-supplied value** (the `source_peer` rule).
  A value any WG-trusted process can write is a value a citizen must
  not trust. SPEC 18 therefore *requires*, but does **not author**,
  an authenticated principal/origin model. That model is **cross-
  cutting** — Ch01 wire envelope + Ch05 §15.2 origin discipline,
  consumed by maild (SMTP/IMAP-authenticated senders), webd (HTTP/WS
  external callers), and future citizens — so it MUST be specified
  once at the substrate level, not minted locally per consumer. SPEC
  18 pins only the invariants it depends on:
    1. **Carried as a dedicated protected envelope field, distinct
       from `source_peer` — not an overload of it.** `source_peer`
       answers *"which mesh peer/transport delivered this"*; the
       authenticated *principal* (and its origin trust-class) is a
       different question and MUST get its own field, so neither
       semantic is widened to cover the other. The leaning is a
       substrate-legible model — a SPEC-12-backed principal registry
       the field references — over an opaque string, so authorization
       is queryable substrate state, not a magic header.
    2. **Same protected-overwrite discipline as `source_peer`.** The
       field is broker-minted on the external→broker hop (webd's
       HTTP/WS ingress is an *external transport*, the analogue of
       Ch05 §15.2's SMTP gateway); the broker's basis for trusting
       webd's classification is webd's **authenticated registered-
       service identity** (`webd`, SPEC-10 UID 502), the role DKIM
       plays for SMTP-injected `source_peer`; and the broker MUST
       overwrite any process-supplied value for every message so no
       WG-trusted process can forge it.
    3. **Exact field name + webd→broker injection mechanism are
       deferred to the first external consumer** (not pre-designed
       here, and not Ch04/Ch05 amendment text this chapter writes).
       The first concrete consumer that needs to distinguish caller
       classes drives the wire-name and registry shape; until then
       only the requirement and the interim safe state below are
       normative.
  webd MUST still enforce per-route **and** per-command authz
  default-deny *before* forwarding, and reject (never forward) a
  request on an external-facing route whose required principal is
  missing/invalid. The citizen MUST treat forwarded external *content*
  as untrusted (§6 "WG-trusted ≠ caller-benign") and MAY use the
  broker-minted principal/origin for finer per-command authorization,
  but MUST NOT trust any unstamped or client-supplied value.
  **Phase-gate (honest dependency, not assumed working):** the broker
  does not mint this field today; defining it is the cross-cutting
  Ch01 + Ch05 §15.2 identity model above, owned downstream and tracked
  in §10.6, **not** authored by this chapter. Until it lands, no
  external webd route may be enabled — the §6 default-deny ("internal
  until a webd route is deliberately added") is the interim safe
  state, not a TODO papering over an open hole.
- **Intra- vs inter-mesh posture.** Same gateway, different
  bind/authz: per `feedback_wg_only_binding`, WG-or-loopback is the
  default; a public/external bind is an edge-node opt-in on
  allowlisted ports, and external-facing routes REQUIRE an explicit
  authz entry distinct from intra-mesh ones (a route may be
  mesh-internal-only).
- **What webd does not do:** business/dynamic logic. That lives in
  citizens. webd is a translator, not an application — keeping it the
  thin, rarely-rebuilt public edge the §0 criteria want.

## §8 GUI / ARexx-widget interaction

> **Retirement note (2026-08-16):** the `ui.*` display lane this section
> describes was retired with chapters 01b/05 (`ui.*` left ABP at the
> control-plane pivot; webd is the agent surface, the desktop stack is
> cosmix-comp + CTK). This section is kept as dated history of the
> ARexx-widget model — do not build a `ui.window` citizen against it. The
> §3.7/Ch05 §7.3 handler-concurrency constraints it leans on remain live
> (see the Ch05 carve-out in §11 Cross-references).

A Mix citizen may also be a UI process (Ch05 ABP Display Protocol): it
`send`s `ui.window` (markdown + widget code blocks), receives widget
events through `on` handlers, and pushes updates via `ui.data`. This
is exactly the AmigaOS application-with-an-ARexx-port model: a
long-lived addressable citizen that *is* an interactive application.

The §3.7 cooperative-loop constraint governs UI responsiveness
directly — `project_mix_yield_question` is, at root, a UI-freeze
concern. A UI Mix citizen MUST keep handlers fast or be Class C
(Phase 2). Display vocabulary is Ch05/Ch06's; this section cross-
references and MUST NOT redefine it.

## §9 Phasing & acceptance

**Phase 1 — Foundation (Class S only).** Serve-mode contract
§3.1–§3.6; reconnect/backoff §3.3 implemented *in
`cosmix-lib-client`*; one canonical reference citizen taken through
the entire SPEC-10 path (UID allocation, sysusers entry, systemd unit
template, register, induced broker bounce, SIGTERM dereg); the
de-risking spike's findings folded back into this chapter before
Phase 2. **The reference citizen MUST be a single-topic-subscribing
state-holder, not an echo/health stub** (normative for the spike): it
subscribes exactly one Ch03 topic in its init body, holds the last
received value as in-process state, and answers one query command
with that state. An echo/health stub exercises only re-*register* on
reconnect and would leave §3.3's actual gating defect — re-
*subscribe* on reconnect — unverified; a state-holder forces it. **The
Phase 1 reference citizen MUST be Class S and intra-mesh only**
(normative): it MUST NOT take an external webd route and MUST NOT
require yield-on-`send`, so the de-risking spike cannot back-door the
#2 concurrency or #3 external-origin amendments into Phase 1.
*Acceptance:* reference citizen (a) runs under its own SPEC-10 UID,
(b) survives an induced `cosmix-noded` restart, re-registers, **and
re-subscribes its init-body topic such that a value published to that
topic *after* the bounce is reflected in the query** (§3.3 — this
proves the subscription reattached, not merely that the socket and
service name did), (c) a handler panic does not kill it, (d) **both**
`SIGTERM` and the `QUIT` universal drive the §3.5 path (deregister
then exit 0), (e) satisfies Ch07 L0+ conformance
(`HELP`/`INFO`/`QUIT`, `<svc>.props.{get,list,describe}`, lifecycle
state).

**Phase 2 — Concurrency (Class C, acyclic only) — SHIPPED 2026-05-28.**
The Ch04 yield-on-`send` amendment landed via the **explicit
`on <cmd> async` modifier** (mechanism choice per §10.3 — explicit
won the implicit-vs-explicit decision; the burden-of-proof was on
implicit and it didn't carry). Co-shipped: a per-`send`
`timeout=<sec>` kwarg (cooperative cancellation; pending-reply slot
freed on timeout). **No reentrancy** — a Class C handler is never
re-entered while suspended; it yields the dispatch reader at every
`send`/`reply`/`sleep_ms` and other invocations of the citizen may
interleave through, but the suspended invocation's frame stays its
own. Scope remains explicitly bounded: this removes head-of-line
blocking for **acyclic** slow-downstream handlers. It does **not**
legalise synchronous cycles — those are prohibited *by design*
(§3.7 deadlock corollary; §10.3 records there is no planned Ch05
§7.3 reentrancy amendment), not pending a future one.
*Acceptance harness:* `_bin/spec18-phase2-acceptance.mix`
(3/3 PASS, 2026-05-28) — Phase A asserts a fast ping interleaves a
slow Class C batch (downstream `slowsvc` HOL-blocker, ~6s serial,
fast↔slow gap > 4s confirms interleave); Phase B asserts the
Class S baseline serialises the fast ping behind the sync batch
(no async, no interleave — pins the lack-of-regression); Phase C
asserts the per-send `timeout=0.5` returns the typed result-var
shape promptly against a 2s downstream — `$rc = "-1"` and
`$result = "timeout: send to <target> exceeded 0.5s"`, mirroring
other transport failures (no new `rc="timeout"` namespace) — and
the citizen recovers for follow-up requests. *Implementation commits:* f60cb2c..d823d4f (WS1 parser
through WS5 harness + bb32074 broker pending-response-id collision
fix). No acceptance claim is made about synchronous cycles; they
remain prohibited.

**Phase 3 — webd boundary.** §7 route table + default-deny authz +
intra/inter-mesh posture; one external route end-to-end with authz.
**Hard dependency, not authored here:** the cross-cutting
principal/origin identity model (§7, §10.6 — Ch01 envelope + Ch05
§15.2 discipline, a dedicated protected field distinct from
`source_peer`) must exist before any external route is enabled. This
chapter does **not** define that model in Phase 3; Phase 3 *consumes*
it. No external route may be enabled before it lands — until then the
§6 default-deny is the safe state.
*Acceptance:* an unlisted route reaches no citizen; a listed
external route reaches its citizen only with its authz entry present;
a citizen observes the broker-minted principal/origin field and a
process-supplied forgery of it is overwritten by the broker (not
believed).

**Phase 4 — Orchestration / MCP / GUI hardening.** §4/§5/§8 patterns
exercised with the Phase-2 **acyclic head-of-line** hazard closed.
Synchronous request cycles remain PROHIBITED (§3.7 deadlock
corollary) — they are *not* unblocked by Phase 2 and stay prohibited
**permanently by design** (§10.3: no Ch05 §7.3 + Ch04 reentrancy
amendment is planned), not pending future work this chapter authors
or anticipates. No Phase 4 acceptance claim is made about synchronous
cycles.

## §10 Open questions / decisions for Mark

1. **Chapter number — DECIDED: keep 18.** This chapter stays at 18.
   Slot 15 ("Daemon Infrastructure") is **not** to be back-filled with
   this content. The 13–17 reserved band is to be **retired or
   renumbered when 13–17 are actually authored** — its disposition is
   downstream of that future work, not a slot this chapter pre-claims
   or vacates now. No renumber of 18 is planned; `README.md`
   reflects this as a decided gap, not an open question.
2. **Ephemeral citizen policy** (§2). Two sub-questions, one
   resolved:
   - *Citizen sub-range — RESOLVED by SPEC-10 v1.2.0 (2026-05-16).*
     SPEC-10 v1.2.0 adds a *citizen-identity entry class* in a
     dedicated **600–699** band, disjoint from the 500–599
     daemon-identity / shared-credential window, governed by
     **scoped, gated reuse** (SPEC-10 §2.2 entry class, §2.5
     lifecycle, §2.3 R7 allocation + R8 conjunctive reuse gate:
     mesh-wide *automated* purge-verification AND a 30-day
     quarantine window). A **registered serve-mode citizen is a
     SPEC-10 citizen-identity entry** — one slot, one unique ABP
     name, one daemon leaf, but **reclaimable** under R7/R8 rather
     than strict-no-reuse like a 500-band daemon slot. The Phase-1
     reference citizen (`statecache`, POSIX `cosmix-statecache`)
     takes the first citizen-range UID **600** (SPEC-10 Appendix A
     v1.2.0). The pre-existing 506/cloudd daemon-stream gap is
     **out of scope** for this resolution — a citizen entry never
     touches the 500–599 stream, so daemon-stream hygiene is
     orthogonal future SPEC-10 work, not a SPEC-18 blocker.
   - *Register vs unregistered — DECIDED LEANING; residual policy
     OPEN (not blocking).* **Decided leaning: ephemeral ⇒
     unregistered by default; registration is the deliberate opt-in
     for request-addressability** (§2 is authoritative — the
     unregistered form is the cheap-many pressure valve, and only a
     workload that genuinely needs a request-addressable name spends
     a SPEC-10 citizen slot). The residual open sub-question is
     narrow: the *policy* of exactly which ephemeral workloads cross
     the threshold into warranting a slot — deferred to the first
     concrete ephemeral consumer (§9 Phase 2/3), not decided here.
3. **yield-on-`send` + reentrancy — FULLY DECIDED & SHIPPED (Phase 2,
   2026-05-28): explicit `on <cmd> async` modifier; per-`send`
   `timeout=<sec>` kwarg; no reentrancy; synchronous cycles
   prohibited by design.** (§3.7.) The Ch05 §7.3 non-reentrancy
   rule is **not** amended — there is no planned
   continuation/reentrancy amendment and synchronous request cycles
   stay prohibited at every phase by design, not pending future
   work. The previously-open mechanism sub-question is **closed**:
   the Ch04 amendment selects the **explicit** form — a trailing
   `async` modifier on the `on` handler header (Class S unmodified;
   Class C opt-in per handler). The decisive consideration that
   carried the day is the one already on record: implicit yield
   would silently and retroactively break the run-to-completion
   atomicity every *already-shipped* Mix script (`sysmon.mix`, the
   CMM scheduler scripts) was authored under — a correctness change
   in code no one would re-audit; the explicit form is additive,
   opt-in, and test-catchable at author time. Co-shipped with the
   modifier: WS4's per-`send` `timeout=<sec>` kwarg (cooperative
   cancellation; pending-reply slot freed on timeout). Implementation
   landed across WS1..WS5 commits f60cb2c..d823d4f; acceptance
   harness `_bin/spec18-phase2-acceptance.mix` (3/3 PASS). Normative
   references: Ch04 §"Event handlers (`on`)" (Class S/C semantics +
   `timeout=`); Ch05 §7.3 rules 6–7 (concurrency model + timeout).
4. **webd route/authz artifact shape** (§7) — **OPEN, Q1; resolve
   before Phase 3, ideally before Phase 1.** Two coupled
   sub-questions:
   - *Artifact count.* §7 describes a **route table** (`(method,
     host/path-pattern) → (ABP service, command, arg-mapping)`) *and*
     a **default-deny authz allowlist** (which route may reach which
     citizen). Are these **one artifact** (a single table carrying
     route + authz columns) or **two** (a route table with a separate
     authz overlay)? This determines what webd loads at startup and
     the operator workflow for adding a new external surface — a
     load-bearing shape decision, not cosmetic.
   - *Representation.* Whichever artifact count: node.toml block vs
     property-substrate SPEC-12 record — a SPEC-12 record is more
     substrate-legible but heavier.

   These are entangled (the representation choice is easier once the
   artifact count is fixed) and worth resolving early: the answer
   shapes webd's config model. **Phase 1 is independent of Q1** (the
   reference citizen is intra-mesh, no webd route), so Q1 does not
   block Phase 1 *coding* — but it should be decided before any
   external-facing citizen design begins (Phase 3), and deciding it
   before Phase 1 avoids a later webd-config rework.
5. **Ephemeral supervision** (§2) — **DECIDED LEANING; residual
   OPEN.** **Decided leaning: ship the `Type=oneshot` (or
   direct-invocation) stopgap; do *not* couple Phase 1 to the
   unbuilt `cosmix-cron`.** Phase 1's reference citizen is a resident
   `Type=simple` unit and needs none of this; ephemeral supervision
   is a Phase 2/3 concern. Residual open sub-question: the migration
   path when `cosmix-cron` lands (stopgap units re-homed under it vs
   left as-is) — deferred to when `cosmix-cron` is actually built.
6. **Principal/origin identity model — DEFERRED, cross-cutting, not a
   SPEC-18 amendment** (§7). This is a substrate-wide identity model
   (Ch01 wire envelope + Ch05 §15.2 origin discipline), consumed by
   maild / webd / future citizens — **specified once at the substrate
   level, not minted per consumer and not authored by this chapter**.
   Decided leanings on record: (a) carry it in a **dedicated protected
   envelope field, distinct from `source_peer`** — do not overload
   `source_peer`, which answers a different question (which peer/
   transport, not which authenticated principal); (b) prefer a
   **substrate-legible SPEC-12-backed principal registry** the field
   references over an opaque string; (c) **defer the exact field name
   and webd→broker injection mechanism to the first external
   consumer** that must distinguish caller classes — do not pre-design
   them. Until it lands, default-deny holds: no external webd route
   may be enabled (the interim safe state, §7). SPEC 18 states only
   the requirement and that safe state.
7. **Synchronous-cycle enforcement honesty — DECIDED (Q2): no broker
   detection required; the contract is the safety mechanism** (§3.7
   deadlock corollary, "Enforcement is contractual, not detected").
   Synchronous request cycles are prohibited permanently by design
   (§10.3), but the broker does **not** detect them today and SPEC 18
   does **not** require it to: a violating cycle deadlocks *silently*.
   The decision on record is that the **contractual prohibition is
   the safety mechanism** — not broker enforcement — and the spec
   states this explicitly so no reader infers enforcement that does
   not exist. Whether the broker should eventually gain cycle
   detection is a **separate Ch01/Ch05 broker-capability question,
   out of scope for SPEC 18** (noted here only so the gap is
   acknowledged, not silently carried).

## §11 Cross-references

- `2026-03-24-01-bus-wire-protocol.md` — wire, trust domain, registered-service
  naming (§3.1, §6); **normative dependency (cross-cutting, not
  authored here)**: the substrate principal/origin identity model —
  a dedicated protected envelope field distinct from `source_peer`
  (§7, §10.6).
- `2026-03-29-02-bus-command-vocabulary.md` — `HELP`/`INFO`/`QUIT` L0 universals
  (`QUIT` → §3.5 graceful path) and command-namespace discipline
  (§3.1, §3.5, §3.6).
- `2026-04-10-03-bus-topic-pubsub.md` — topic fan-out for orchestration; topic
  re-subscription on reconnect (§3.3, §4).
- `2026-04-13-04-mix-language-reference.md` — Mix grammar (§"Event handlers
  (`on`)" owns the `on` form); **normative dependency**: the §3.7
  yield-on-`send` amendment.
- `2026-04-07-05-amp-display-protocol.md` §7.3 — cooperative single-threaded,
  **non-reentrant** handler model (§3.7, §4; non-reentrancy is
  permanent by design — §10.3); §15.2 — origin tracking /
  `source_peer` protected-overwrite rule, the *discipline* the §7
  principal/origin field follows (a **distinct** field, same broker-
  mint/overwrite rule — not a `source_peer` overload); `06_cosmix-
  display-model.md` + Ch05 — UI vocabulary (§8). *(Ch05 was retired
  2026-08-16 — `ui.*` left ABP at the control-plane pivot — but its §7.3
  handler-concurrency and §15.2 origin-discipline clauses remain the
  normative reference for this chapter until they are inlined here; the
  retirement covers the display surface, not these contracts.)*
- `2026-04-27-07-self-aware.md` — daemon conformance: §9 L0 (`HELP`/`INFO`/
  `QUIT`) + `<svc>.props.{get,list,describe}` + lifecycle state
  (§3.5, §3.6).
- `2026-05-09-10-cosmix-daemon-identity.md` — **companion**: identity & OS
  lifecycle (§2), consumed not duplicated.
- `2026-05-11-12-property-substrate.md` — candidate store for webd authz (§10.4).
- Planned ch.15 "Daemon Infrastructure" — relationship per §10.1.
- Memory: `project_mix_yield_question` (§3.7),
  `feedback_amp_wire_trust_boundary` (§6),
  `feedback_wg_only_binding` (§7), `feedback_pre_exec_pdeathsig`
  (§2), `project_agent_runtime_unification` (§5),
  `feedback_refactor_silent_noop_audit` (§3.3, §5).
- `CODEX.md` — the cold-review framing this draft is written to
  survive.
