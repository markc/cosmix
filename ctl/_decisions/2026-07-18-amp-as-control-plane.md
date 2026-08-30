# ADR: ABP is the control plane — retiring the display-protocol rendering path

- **Date:** 2026-07-18
- **Status:** OWNER-DECIDED (Mark, 2026-07-18) — "Yes, locked in." Drafted by Claude
  (fable-5) for Mark's review; adversarially converged with codex (thread
  `019f7335`, two rounds: gui-grid strategic review + control-plane pivot).
- **Decision authority:** Mark — direction-setting call ("what should Cosmix
  become" class). The pivot-round recommendation was unanimous (codex round 1
  supplied the premises — two-frontend-model finding, spoofing risk, foundation
  price tag — without itself recommending retirement; round 2 recommended
  adopt).

---

## 1. Decision

ABP returns to its original ARexx lineage: a **MIDI/OSC-like control protocol**.
It controls apps and remote services; it does not paint pixels. The `ui.*`
display protocol — ABP messages as the *rendering path*, with domain-blind
renderers drawing windows/widgets from wire vocabulary — is **retired as a
substrate primitive**.

The stack in one breath:

> **Daemons own domains. ABP is the control plane (verbs + SPEC-12 props +
> event/telemetry streams). Apps render natively with whatever engine, but must
> expose an honest app-control port. webd is the agent-authored UI surface.
> Mix scripts drive everything.**

`mixer.v1` (addressable leaves + write wire + binary telemetry stream) is the
canonical shape of an ABP control surface — an OSC-style address space. The
pivot generalizes what already worked and retires the one lane that fought us.

## 2. Context — what triggered this

1. **Security.** ABP-rendered windows are a spoofing surface: any authorized
   mesh peer could paint trusted-looking UI, including credential prompts.
   The **rendering-specific** part of the bill — remote-DOM vocabulary and
   capability negotiation, a11y-over-wire, IME/focus semantics, a headless
   canonical interpreter, renderer conformance suites, window-painting
   spoofing itself — is eliminated by this pivot. The **control-plane** part
   — per-verb capability authority, action provenance, confirmation flows —
   is *not* eliminated; it returns, smaller and tractable, as B-3.
2. **Lived evidence.** The disp-skia apps went unused for months while all
   real UI work (and all agent-authored UI) happened in webd. The
   monopoly-pause ADR (2026-07-15) had already flagged the responsiveness
   ceiling and productivity tax.
3. **Architectural incoherence.** The gui grid held two irreconcilable
   frontend models: skia as a domain-blind `ui.*` renderer vs bevy as a native
   domain client linking `cosmix-mixer-schema` directly (egui/iced/html arms
   linked neither). "Interchangeable renderers" was true of one arm. Under
   this ADR the bevy model becomes the norm, not the anomaly.

## 3. Retired / kept

**Retired:**
- The `ui.*` vocabulary as a rendering protocol (`ui.window/style/remove/
  event/theme/value/telemetry/action` as *the way UIs exist*).
- `cosmix-lib-display` as a protocol crate. Retire the name; do **not**
  repurpose it (a shrunken "control vocabulary" under the old name invites
  drift back).
- The disp-skia rendering path and the skia mixer broker's A.6→MeterBank
  transcoding role (archive, own timeline — §8 P3).
- The domain-blind renderer rule (B1/Q1) and generic-frame parity as
  bake-off requirements.
- The renderer-conformance-suite ambition (replaced by app-port conformance:
  the B-2 invariants, built as a harness in §8 P3).

**Kept, untouched:**
- The ABP wire protocol, SPEC-12 props, all daemon verb surfaces.
- `cosmix-mixer-schema` — promoted from bake-off keystone to the canonical
  pattern for domain control contracts.
- benchd + the parity hash + trace corpus (repurposed, §6).
- webd as the agent-authored/admin surface; the native engine arms as
  candidates; Mix as the ARexx layer.
- The permanent "no third-party GUI *application framework*" rejection is
  **narrowed to the shell**: engines (Bevy today) are now legitimate as
  app-rendering substrates behind control ports; nothing third-party becomes
  the substrate's own UI model.

## 4. Boundaries (from adversarial review — these are load-bearing)

- **B-1 App ports expose app-local semantics only** — activate, navigate,
  select, inspect current view. Domain work belongs to the daemon's verb
  surface: agents send maild the query, not the mail app. The app's own
  widgets do domain work by invoking the same semantic commands that hit the
  daemon (B-2), but the app *port* must not grow a second domain API.
  Controllability is therefore judged on the **daemon + app-port composite**,
  never the app port alone. Otherwise the fat-client problem is rebuilt
  sideways.
- **B-2 One semantic command path.** Local input and remote control invoke
  the *same* semantic commands, whose durable/domain effect is owned and
  validated by the daemon. "Every GUI action reachable via the port" alone is
  gameable by an automation veneer over fat native logic. Conformance tests:
  GUI-triggered and direct-daemon commands converge to identical
  authoritative state; app restart loses only declared-ephemeral view state;
  two concurrent clients converge; the GUI holds no durable domain store or
  alternate validation rules.
- **B-3 Control authority is risk-classed, not flat.** Spoofing shrinks but
  doesn't vanish: a remote controller can still raise an app, fill fields,
  and steer a user into a deceptive confirmation. Control verbs carry
  side-effect/risk classification and provenance; sensitive ones require
  local confirmation. Some operations are **enumerated local-only with a
  stated security reason** (password entry, file-picker grants, permission
  prompts, destructive confirmations) — a literal "everything remotely
  reachable" rule is wrong. **This is currently a standing unenforced
  assumption:** CTK's port today accepts every broker-verified mesh peer and
  defers per-verb authorisation (`$COSMIX/gui/ctk/src/app_control.rs`).
  The P2 spec must name the enforcement point and the fail-closed default;
  P3 implements it **before any app port becomes normative**.
- **B-4 CTK's control derivation inverts.** Deriving agent controls from
  widget classes ("drive widgets as the user would") is the wrong direction:
  widgets call semantic commands; remote requests call those same commands.
  Agents never need to know something is a fader.
- **B-5 webd becomes security-critical** as the agent-authored surface: CSP,
  output escaping, session isolation, provenance/capability controls are
  part of the trust story, not hygiene.
- **B-6 No substrate rendering primitive returns through the back door.**
  If an agent-conjured window is ever needed beyond webd, it is one ordinary
  trusted native app (a markdown viewer with provenance chrome), not a
  protocol.

## 5. New artifact: the app-control contract (amp repo)

The "ABP app-control contract" named-but-unspecified in the monopoly-pause
ADR §4a is now scoped and becomes the only **ABP-native application-control
model** (rendering itself remains native-app or web/webd). A small contract,
spec'd in the amp repo:

- `app.describe` / `app.activate` / `app.status` / `app.quit`; optional
  document/view navigation.
- Machine-readable verb, property, and event schemas (agent-legible —
  design criterion 1).
- Capability requirements, side-effect/risk classification, idempotency
  declarations per verb (feeds B-3).
- Prefer "activate app/view" over window handles — semantic addresses are
  stabler than `raise window 17`.

## 6. The mixer bake-off, reframed

Retains its value as a **native-client economics and controllability
bake-off**, not a display-protocol contest. Keep: identical `mixer.v1`
backend, final-state parity hash, scripted semantic sessions — split per B-1:
domain operations drive `musicd` directly, app-local operations drive
`app.*`, and injected physical input separately measures the GUI's routing
of the same semantic commands — input→daemon and daemon→pixel latency, power,
memory, startup, build size, a11y and interaction quality. **Add: a
direct-daemon baseline arm** (no GUI) so the GUI/app-port overhead is
measured, not assumed.

**Addendum (Mark, 2026-07-18 — same day, after the pivot review): add a
fused arm.** `mixer-fused` links the bevy board and the musicd mixer
engine into ONE process (identical ctk UI, transport seam swapped from
ABP to function calls) so the process boundary itself is measured, not
argued. The three-arm triangle — direct-daemon baseline / split /
fused — prices the GUI, the control plane, and the process boundary
separately. The fused arm is a measurement instrument, not a product
direction; this ADR's decision stands regardless of its numbers, which
inform the "what does the split cost" record. (Spec was
`_plan/2026-07-18-fused-bench-arm.md`, shipped + deleted per plan lifecycle;
the record below and `_journal/2026-07-18-fused-bench-arm-verdict.md` carry
everything citable.)

**Result (same day, measured — `_journal/2026-07-18-fused-bench-arm-verdict.md`):**
identical board + Pacific stems, split vs fused: issue→ack p50 ≤16 ms →
≤0.2 ms (the boundary itself; wire RTT ~3.8 ms, rest is per-frame drain
quantization); issue→applied (ear proxy) p50 ≤48 ms → ≤32 ms — one frame;
writes/s 44.0 → 43.9 (frame-bound on both); frame p50/p99 24/24 both; RSS
≈1121 MB (three processes) → 1045 MB; CPU ≈68% total → 68.8% (parity).
The split's price is one extra process and ~16 ms of observed ack that the
pipeline already renders irrelevant; **nothing audible**. The 2026-07-18
prediction ("low single-digit ms control latency, zero audible difference")
is confirmed. This ADR's split-architecture decision stands, now on
measured rather than argued ground.

The bake-off still does **not** pick the desktop engine by itself: a
**document-heavy companion trial** (mail inbox + message view + compose, or a
10k-row admin table with filtering/editing) gates any suite-wide default,
with IME, clipboard, keyboard-only operation, resize, and idle/active power
as pass criteria. Bevy's standing: leading GPU-surface candidate; promoted
only after passing both workloads and surviving one upstream upgrade without
material rework.

## 7. Supersedes / amends

(All four superseded docs below were retired from the repo in the 2026-07-23
decisions triage — full text in git history.)

- `amp-first-ui-architecture.md` (2026-04-06) — core thesis ("ABP is the
  event loop, the renderer is the output stage") **reversed**.
- `2026-07-15-amp-display-monopoly-pause.md` §4a — **completed and
  extended**: the "ABP-Display as default for simple surfaces" residual role
  is removed; simple surfaces go to webd or ordinary native apps.
- `2026-07-14-display-renderer-bakeoff.md` + `2026-07-14-mixer-bakeoff-
  harness-design.md` — selection question **reframed** per §6; the harness
  and contract crates carry over.
- `2026-07-15-bevy-ctk-native-surface-candidate.md` — direction **affirmed**
  (native render + ABP citizen is now the only ABP-native application-control
  model; rendering remains native-app or web/webd); its widget-derived
  control approach inverts per B-4.
- `_doc/2026-07-16-bevy-desktop-viability-ctk-takeaways.md` —
  its "CTK must not creep into desktop-suite scope" guard **stands** until
  the §6 document-heavy trial passes.
- `2026-04-27-windowing-vocabulary.md` — moot (no substrate windowing vocabulary to
  name).
- `_spec/2026-04-27-01b-amp-ui-vocabulary.md`, `_spec/2026-04-07-05-amp-display-protocol.md` —
  to be archived with pointer headers (follow-through P2).

## 8. Follow-through (phased; only P1 in this session)

- **P1 — record (this session, for Mark's review):** this ADR; superseded/
  amended headers on the six cmctl docs above plus the cos gui/bevy
  takeaways digest; Claude memory updates; no code changes, no deletions.
  Docs only → no version bumps.
- **P2 — contract:** draft the app-control spec in the amp repo (§5),
  including the B-3 authority model — enforcement point named, fail-closed
  default, risk classes, local-only inventory format; archive specs 01b/05
  with pointers; spec CHANGELOG entry.
- **P3 — code dispositions (each its own reviewed change):** build the
  **app-port conformance harness** implementing the B-2 invariants (shared
  semantic command path, restart reconstruction, concurrent-client
  convergence, no durable client store, local-only exception inventory) and
  the B-3 enforcement; invert CTK's control derivation (B-4); deprecate
  `cosmix-lib-display` and the skia broker's transcoding; archive the
  disp-skia app surface; decide `cosmix-mail`'s fate (port to webd or
  archive — it is the only in-tree `ui.*` app); re-point the gui-grid arms
  at the app-control contract.
- **P4 — bake-off rerun** under §6 terms + the document-heavy companion
  trial, **gated on the P3 conformance harness** (no arm is measured as
  "controllable" without passing it); then, and only then, an engine-default
  decision.

## 9. What this ADR does not decide

The desktop engine default (gated on §6); the fate of each individual gui
arm beyond the skia broker; the app-control spec's wire details (P2, its own
review); webd's hardening roadmap (B-5 names the posture, not the work plan).
