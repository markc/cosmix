# ADR: The Interaction Service — a source-agnostic dialog/notification broker

- **Date:** 2026-07-20
- **Status:** PROPOSED — awaiting Mark's direction-setting call. Recovered and
  transcribed by Claude (Opus 4.8) from a codex research-and-design pass that
  otherwise lived only in a rollout log (session
  `019f7557-15a7-7a52-8097-1f244f727893`, 2026-07-18, run from `$CMCTL`; codex
  `gpt-5.6-sol`). **That pass changed no files** — this ADR is the first written
  record of the design. **Adversarially reviewed by codex 2026-07-20 (thread
  `019f7e3f`) — verdict BUILD SMALLER-FIRST; findings and the revised
  recommendation are in §12, which supersedes the original §9 sequencing.**
- **Decision authority:** Mark — this is a "what should Cosmix become" call (a
  new session-level daemon, a new ABP verb family, and a cross-app UI contract).
- **Phase C update (2026-07-22):** Phase C (the resident broker + ABP surface,
  §12 revised recommendation) now has a concrete proposal — **ctkd**, in
  `_doc/2026-07-22-ctkd-ephemeral-surfaces-daemon.md`. It does
  **not** lift the three BLOCKERs below; it sharpens the daemon shape (a
  `[[bin]] ctkd` inside the `ctk` package) and adds a `cosmix-lib-mesh-trust`
  authz mechanism that closes **remote-peer** authorization only — BLOCKER-1
  (on-wire confidentiality), BLOCKER-2 (local principal spoofing / B-3), and
  BLOCKER-3 (session-qualified naming) still gate the surface.
- **Phase C SHIPPED (notify.v1 subset) — 2026-07-22/23:** the cos-side ctkd ADR
  §10 re-dispositioned the BLOCKER decisions and a Phase-C daemon shipped in
  $COSMIX: **cosmix-interactd 0.1.0** (headless, `src/crates/` — placement
  re-decided in the notify-v1 spec §11 because notify.v1 is renderless; the
  `[[bin]] ctkd`-inside-ctk shape above was superseded), registering ABP service
  **`interact`** with `interact.notify/update/dismiss/props.*`, a FreedesktopSink
  (notify-rust/zbus), and action click-dispatch. The audited implementation
  shipped as **cosmix-interactd 0.2.0**; current ownership, rate, and property
  enforcement lives in `cosmix-interactd/src/{state,props}.rs`. The
  **dialog/modal surface remains gated** — only notify.v1 is live.
- **Relationship to neighbours:**
  - **Extends** `2026-07-18-amp-as-control-plane.md` — ABP is the control plane;
    apps render natively but expose an honest app-control port. This ADR proposes
    one such port: an `interact.*` verb family for *asking a human a question*.
  - **Distinct from** `2026-07-20-desktop-event-flow-and-action-broker.md`. That
    ADR's "Action broker" is the **input side** — routing keyboard/mouse/ABP/MIDI
    into one `ActionRequest` bus inside a Bevy app. This ADR is the **output
    side** — presenting a dialog/notification and collecting a human answer. They
    are complementary, not competing: an `Action` might *raise* an
    `InteractionRequest`; the interaction's result might *fire* an `Action`. Keep
    the two type families separate (see §7).

---

## 1. Why now

Two Bevy apps already grew dialog machinery independently:

- **CTK's file requester** (`$COSMIX/gui/ctk/src/file_requester.rs`) has a queue,
  a modal focus latch, and result correlation.
- **Fable's browser** reimplements destructive-action confirmations and overlays
  separately.

That is the same widget built twice, diverging. Meanwhile Mix scripts and
background daemons have **no** way to ask a human anything — there is no Cosmix
equivalent of `kdialog`/`zenity` for "confirm this deletion", "enter a
passphrase", or "here's a progress spinner for a long job". Every future need
(a settings dialog, a provisioning confirm, a maild passphrase prompt) is
currently a bespoke re-implementation.

The design criteria pull toward a broker: a named, queryable set of pending
interactions is **legible**; driving one over ABP from Mix is **modifiable**;
an agent enumerating and answering interactions is **reconstructible**. `kdialog`
is the proof-of-concept for the *shape* (a CLI/RPC that returns a typed answer),
not a dependency to adopt.

## 2. The core decision

Build a **Cosmix Interaction Service**, not merely a shared dialog crate. It is
the single broker for "the system needs a human to see or decide something,"
regardless of *who* asks (Bevy app, Mix script, daemon) or *how* it is shown
(embedded overlay, standalone window, desktop notification).

Three cooperating layers, one broker:

```text
Mix scripts ───────┐
Cosmix services ───┼──── ABP interact.* ──── cosmix-interact broker
Bevy applications ─┘                              │
                                      ┌───────────┼──────────────┐
                                CTK embedded   standalone     desktop
                                  presenter    Bevy window   notification
```

The separations are load-bearing:

- **Application-modal** dialogs must render *inside the owning app's* Bevy window
  — a central process cannot inject a modal overlay into another process's
  window.
- **Mix scripts / background services** have no window of their own, so they need
  a **standalone** host.
- **System-wide passive** messages should use the existing desktop notification
  service, not a bespoke toast window (Wayland won't reliably position one).
- **Clipboard is explicitly out of scope** — different security and
  data-lifetime rules; it becomes a separate `clipd`/`clip` service later (§6),
  not part of this broker.

"System-wide" means **one broker per logged-in graphical user session** (under
`systemd --user`), never one root daemon for the whole machine.

## 3. Proposed components

### 3.1 `cosmix-interaction-schema` (pure Rust, headless workspace)

`$COSMIX/src/crates/cosmix-interaction-schema/` — serialisable types, no Bevy
dependency, unit-testable (core-and-citizen pattern):

```rust
struct InteractionRequest {
    owner: String,               // ABP service identity of the caller
    scope: InteractionScope,     // Application | Session | Desktop
    presentation: Presentation,  // Embedded | Standalone | Desktop | Auto
    kind: InteractionKind,       // message | confirm | input | choice | file | progress | toast | notification
    title: Option<String>,
    message: Option<String>,
    severity: Severity,
    actions: Vec<ActionSpec>,
    dedupe_key: Option<String>,
    remember_key: Option<String>,
    deadline_ms: Option<u64>,
    cancellable: bool,
    sensitive: bool,             // INTENT ONLY — not a system property today; see §12 BLOCKER-1
}

struct ActionSpec {
    key: String,
    label: String,
    role: ActionRole,            // Accept | Cancel | Destructive | Help | Auxiliary
    default: bool,
}
```

The broker assigns a **`handle`** (deliberately *not* `id` — ABP already uses
`id` for request correlation). Lifecycle, exactly one terminal state per
request:

```text
queued → presented → resolved
                   ↘ cancelled
                   ↘ expired
                   ↘ failed
```

### 3.2 `ctk::interaction` (reusable CTK plugin)

`$COSMIX/gui/ctk/src/interaction/` owns the embedded presenter: modal queueing,
overlay + backdrop, keyboard focus trapping and return-to-invoker, Escape/Cancel,
default vs destructive action selection, toast stacking + dedupe, progress/
spinner, accessibility roles, result messages. **The existing CTK file requester
and Fable dialogs converge onto this** — the file requester stays specialised in
directory enumeration/filtering/save-name validation but uses this layer for
presentation and completion. For destructive dialogs, focus the *least*
destructive action first (WAI modal-dialog pattern; maps to AccessKit `Dialog`/
`AlertDialog`/`Alert`).

### 3.3 `cosmix-interact` (standalone Bevy binary)

`$COSMIX/gui/apps/interact/` → `~/.local/bin/cosmix-interact`, two modes:

```sh
cosmix-interact serve          # registers ABP service `interact`, no window until needed
cosmix-interact show confirm --title "Delete asset?" --message "/path/to/file"
cosmix-interact show progress --indeterminate --title "Loading assets"
```

Structured JSON on stdout; suggested exits `0` accepted/action, `1` cancelled,
`2` expired, `10+` validation/policy/renderer error. This borrows KDialog's
CLI-convenience model **without** its shell-output-parsing fragility, and its
long-lived-progress model (return a handle, update/close it later).

### 3.4 ABP interface

```text
interact.show          create and wait for a result
interact.open          create and immediately return a handle
interact.update        update message / progress / available actions
interact.get           query state
interact.list          list caller-visible interactions
interact.cancel        request cancellation
interact.close         close with a supplied outcome
interact.capabilities  report presenters and supported features
```

Events: `interact.changed`, `interact.resolved`, `interact.action`. From Mix:

```mix
$r = send interact interact.show kind="confirm" title="Delete asset?" message=$path
if $rc == 0 and $r.action == "delete" then
    # perform the deletion
end
```

```mix
$p = send interact interact.open kind="progress" mode="indeterminate" title="Loading assets"
$h = $p.handle
send interact interact.update handle=$h message="Scanning SoundFonts"
send interact interact.close  handle=$h outcome="succeeded"
```

One modal lane per owner/window; queue extra modal requests. Progress/toasts may
be non-modal. Cap visible toasts at ~3, coalesce by `dedupe_key`.

### 3.5 Presenter registration (the hard part)

Because no process can place a modal inside another's window, each CTK app
eventually registers as a presenter:

```text
app.interaction.register
app.interaction.present
app.interaction.resolve
```

An `Application`-scoped request routes to e.g. Fusion/Fable, which shows its
local CTK overlay and reports the result. If the owning app is unavailable, the
broker falls back to a standalone `cosmix-interact` window. Focus/authorization
is **explicit broker state** — Cosmix has no Wayland per-client focus model to
lean on.

## 4. Presentation policy

`embedded` (CTK overlay in the owning app) · `standalone` (separate window) ·
`desktop` (freedesktop notification) · `auto` (broker chooses by kind, ownership,
presenter availability). Constraints that shape this:

- `cosmix-interact` is a **client** of the existing notification service — it
  must **not** claim `org.freedesktop.Notifications`. Desktop notifications are
  passive and exclude modal boxes: use them for information/optional actions,
  never as the only way to complete an operation; use `replaces_id`/a stable key
  for progress updates.
- Wayland forbids apps positioning toplevels at arbitrary coordinates, so
  custom "bottom-right toast windows" are unreliable — use desktop notifications
  for genuinely system-wide toasts.
- Rust adapters: `notify-rust` first, `ashpd` (XDG portal) backend later for
  sandboxed callers. The portal's handle-then-response pattern (interaction can
  exceed D-Bus timeouts, needs cancellation) is exactly why the ABP interface
  uses handles too.

## 5. Security and policy

Session broker under `systemd --user`; accept local ABP callers by default;
attribute every request to its **actual** ABP service identity; prevent callers
spoofing system/security prompts; reject remote-mesh dialogs unless target node +
user + capability are explicit; no focus stealing; sensitive bodies in memory
only (**aspirational — see §12 BLOCKER-1: local ABP is mirrored to `noded.tap`,
so no confidentiality exists on the wire today**); persist only preferences (quiet mode, remembered answers, file-purpose
dirs); rate-limit repeat-notification callers. Persistent notification actions
route to a **registered service**, not an originating process that may have
exited.

## 6. Explicitly out of scope: clipboard

Do **not** make the interaction broker the clipboard owner — passwords, copied
files and large binaries must not mix with dialog metadata or logs. A separate
future `cosmix-clipd` (ABP service `clip`) handles system clipboard, primary
selection, and **typed Cosmix action buffers** (e.g. a `FileTransferIntent`
`{operation, sources, source_app, created_at}` so a file move is a typed intent,
not anonymous bytes). Noted here only to keep it firmly *out* of this broker.

## 7. Two brokers, kept apart

| | Action broker (`2026-07-20-desktop-event-flow…`) | Interaction Service (this ADR) |
|---|---|---|
| Direction | **input** → app | app → **human** → app |
| Spine type | `ActionRequest { action, source, value }` | `InteractionRequest { owner, kind, … }` |
| Lives | inside one Bevy app | session-wide broker + per-app presenters |
| Verb | `action.invoke` | `interact.show` / `interact.open` |

They compose (an Action may raise an interaction; a resolved interaction may fire
an Action) but the type families stay separate — collapsing them recreates
exactly the "one ambiguous API" this design is trying to avoid.

## 8. Initial scope

Kinds: `message`, `confirm`, `input` (incl. password), `choice`, `file`,
`progress`, `toast`, `notification`. **No arbitrary custom forms** in v1 — that
becomes another widget-description protocol and overlaps webd/ABP-Display.

## 9. Implementation order

1. `cosmix-interaction-schema` — lifecycle, validation, queue tests.
2. CTK embedded presenter (message/confirm/progress/toast); refactor Fable's
   dialogs + CTK's file-request lifecycle onto it.
3. Standalone broker — `cosmix-interact serve`, ABP verbs, Mix CLI, user service,
   `~/.local/bin` install.
4. Desktop notification adapter — capabilities, replacement, dedupe, urgency,
   optional actions.
5. Application presenter registration — route app-scoped requests into
   Fusion/Fable rather than opening unrelated windows.
6. (Later, separate ADR) `cosmix-clipd`.

## 10. Viability (codex's own first-pass assessment)

| Feature | Viability |
|---|---|
| Embedded CTK modal dialogs | High |
| Loading spinner/progress handles | High |
| Standalone dialogs for Mix | High |
| KDE/system notifications | High |
| Custom system-corner toast windows on Wayland | Poor — use notifications |
| True modal over an arbitrary external app | Medium — requires presenter registration |
| Basic clipboard get/set | High |
| Universal Wayland clipboard history | Medium–low |
| Typed copy/move/action buffers | High |

The CTK file requester and Fable dialog code already prove most of the embedded
mechanics. The largest design risk is **not** rendering — it is keeping modal
interactions, passive notifications, clipboard data and structured app actions
from collapsing into one unsafe, ambiguous API.

## 11. Decision points for Mark

1. **Adopt the broker shape** (session-wide `interact` service + per-app CTK
   presenter + standalone host), or start smaller as *just* a shared
   `ctk::interaction` crate that unifies the file requester + Fable dialogs, and
   defer the ABP/standalone/notification layers?
2. **Presenter-registration model** — is routing app-scoped modals back into the
   owning Bevy app (with standalone fallback) worth the complexity now, or ship
   standalone-only first?
3. **Sequencing vs the Action broker** — the two ADRs share Fusion as first
   consumer. Interleave, or land the input-side Action spine (already scaffolded
   in `action.rs`) before starting this?

## 12. Codex review outcome (2026-07-20) — verdict: BUILD SMALLER-FIRST

Cold adversarial review (codex `gpt-5.6-sol`, thread `019f7e3f`), checked
against the live `$COSMIX` tree. It **does not reject the design** — it rejects
building the resident session-wide broker *first*, because the broker has no
demonstrated consumer yet while the embedded pain is real and present. This
section supersedes §9's "start with the schema/broker" sequencing. Findings, by
severity, with disposition:

**BLOCKER-1 — "sensitive" ABP interactions are not confidential.** Every local
ABP message (headers + body) is mirrored to `noded.tap`
(`cosmix-noded/src/noded.rs`); the standard logger records only body *length*,
but any same-node tap subscriber gets the raw secret. So `input`(password) over
ABP leaks. *Disposition: accepted — the `sensitive` flag is now marked
intent-only in §3.1/§5. Password/secret prompts stay in-process CTK only; they
do not go on the ABP surface until ABP has a confidential-payload + tap-redaction
contract.*

**BLOCKER-2 — prompt authenticity / local-only authority is asserted, not
enforced.** `owner` is caller-supplied; ABP supplies only a registered `from`;
anonymous Mix calls have no durable principal. A local citizen could raise a
trusted-looking "system password required" prompt. The control-plane ADR's
authority layer (its B-3) is explicitly *not yet built*. *Disposition: accepted —
the ABP broker must not ship before B-3 enforcement exists; identity must be
derived server-side, requester separated from target, and provenance chrome be
broker-owned and non-spoofable. Gates the whole ABP surface, not the embedded
crate.*

**BLOCKER-3 — one global `interact` service name can't be "one broker per
session".** `noded`'s service-name registry is node-global and rejects a second
owner; there's no seat/session-qualified routing. Second logged-in user can't
start a broker; an earlier local process can squat the name. *Disposition:
accepted — v1 is constrained to a single graphical session, OR the session
boundary is a per-user Unix socket / D-Bus service with ABP added later via a
gateway. §2's "one broker per session" is not achievable through a bare global
ABP name.*

**MAJOR findings** (all accepted; they reshape, not kill):
- **Presenter routing lacks identifiers/leases.** `owner` alone can't say *which*
  Fusion instance presents; needs immutable requester principal, concrete target
  instance/session, a presenter lease/generation, a one-time attempt token, and
  compare-and-set terminal resolution — else stale-presenter + standalone
  fallback produces duplicate prompts or two resolutions. (Latency is *not* the
  trap — ABP RTT ~3.8 ms + one frame is nothing beside human time; lifecycle and
  focus are the traps.)
- **Caller can bait-and-switch the human.** `interact.update` (change actions) +
  `interact.close` (supply outcome) let a caller open a confirm and instantly
  self-accept it, or mutate labels after presentation, or reuse a `remember_key`
  to silently approve a *different* destructive op. Fix: only the active
  presenter resolves decision prompts; freeze title/message/actions after
  presentation; restrict caller `update`/`close` to progress-style kinds; defer
  remembered decisions (or bind them to requester+fingerprint+risk+expiry).
- **Drifts from substrate-first into bespoke CRUD.** `interact.get/list/
  capabilities/changed/resolved` with no SPEC-12 namespace contradicts the
  mandated pattern. Fix: ephemeral interactions as an `interactions` memory-backed
  props collection; prefs as a persisted per-user namespace; discovery/watch via
  `interact.props.{list,get,describe,watch}`; keep only thin atomic verbs
  (`open`, restricted `update`, `cancel`, presenter-only `resolve`).
- **The schema can't express its own v1.** The struct carries only generic text +
  buttons — no per-kind payload/result for file (mode/filter/default-name), choice
  (values), input (validation), or progress (range/value/`mode` — which the CLI
  example uses but the struct lacks). Fix: shrink v1 to `message` + `confirm`, or
  define a tagged request/result enum driven by real consumers. The "eight kinds"
  of §8 are not actually designed.
- **`interact.show` is the wrong wire primitive.** Holding an ABP request open
  until a human answers pins an entry in `noded`'s unbounded pending-response map.
  Fix: `open` is the only wire primitive; "show and wait" is a client-side `open`
  + props-watch/poll with a bounded timeout.
- **Modal ownership overlaps the Action broker.** The event-flow ADR already makes
  per-app `UiMode`/`ModalStack` the sole capture authority. Fix: `ctk::interaction`
  *drives* that per-app stack rather than owning capture; a resolution is an
  app-local semantic event the app may map to an Action — **the session broker
  never fires Actions itself.** Also: standalone fallback is *not* equivalent to
  app-modality (can't block the origin app; a background process can't reliably
  grab Wayland focus) — if the target app is gone, cancel/fail unless the request
  explicitly permits standalone.

**Confirmed correct (MINOR):** the Wayland arbitrary-positioning claim (winit
0.30.13, still unsupported → notifications are the right route for corner toasts);
and `handle` ≠ ABP `id` (ABP `id` is transport correlation, rewritten broker-side;
an interaction handle must be an opaque, unguessable, stable object id).

**Other stale-claim corrections folded in:** `interact` binary path is
`gui/apps/interact/` (fixed §3.3); "existing notification service" = the *desktop
environment's* external `org.freedesktop.Notifications` — there is **no** Cosmix
notification adapter today, `notify-rust`/`ashpd` would be new deps; "exactly one
terminal state" and "explicit broker focus/authorization state" are intentions,
not specified mechanisms. (Separately: `CODEX.md` still names the pre-reshape
`gui/toolkits/bevy/ctk` path — this ADR is fresher; fix CODEX.md's crate map.)

### Revised recommendation (replaces §11's open framing)

**Phase A — ship `ctk::interaction` now.** A shared CTK plugin: modal shell,
queue, focus trap + return-to-invoker, Escape, accessibility, typed completion.
Refactor Fable's two hand-built overlays (`$COSMIX/gui/apps/fable/src/browser.rs`,
which lacks queue/focus/a11y) and the CTK file requester onto it. This is the
whole evidenced payoff and carries none of the three BLOCKERs (all in-process, no
ABP, no secrets on a wire, no service-name contention).

**Phase B — one-shot standalone native dialog**, added only when the first real
windowless Mix/daemon caller appears. `message`/`confirm` only; no resident
service.

**Phase C — resident session broker + `interact.*` ABP surface**, only once (a) a
second interaction source proves cross-source policy is needed, AND (b) the
control-plane authority layer (B-3) is enforced, AND (c) a session-qualified
identity/discovery story exists. Until all three, the broker is speculative
gold-plating.

Net: the *design* is sound and worth keeping on record; the *first build* is the
in-process crate, not the daemon.

---

## Provenance

Design content transcribed verbatim-in-substance from codex rollout
`~/.codex/sessions/2026/07/18/rollout-2026-07-18T23-07-49-019f7557-…jsonl`
(final synthesis message; "No files were changed during this research and design
pass"). Sources cited by codex in that pass: KDE KDialog docs, freedesktop
Desktop Notifications spec, XDG Desktop Portal architecture, WAI-ARIA modal
dialog pattern, winit/Wayland positioning docs, `notify-rust`/`ashpd`/
`ext-data-control-v1`, Bevy clipboard docs.
