# ADR: Desktop event flow — the Action broker, modal stack, and audio-intent bus

- **Date:** 2026-07-20
- **Status:** **ACCEPTED — Mark ratified §10 on 2026-07-21.** Drafted by Claude
  (Opus 4.8) from three parallel research streams (Bevy 0.19 event model, KDE
  Plasma/KWin/KGlobalAccel, System76 COSMIC/cosmic-comp/smithay). A cold codex
  review of the Phase-0 scaffold (§9) found it doesn't deliver its three headline
  claims; a 3-round codex convergence loop on the resolution itself (§10) reshaped
  the design — the ratified architecture is **"Fusion action ingress above a
  CTK-owned versioned transport lifecycle"**, not "the broker owns the engine".
  **Phase 0 SHIPPED 2026-07-21** (ctk 0.11.4, fusion 0.1.14) — all four stages on
  $COSMIX main; see §11 for what shipped and what was deliberately deferred.
- **Decision authority:** Mark — this is a "what should Cosmix become" call
  (foundational event/input/window architecture shared across apps and the
  future ABP-mesh desktop).
- **Extends:** `2026-07-18-amp-as-control-plane.md` (ABP = control plane; apps
  render natively but must expose an honest app-control port). This ADR says
  *what that port is made of*.

---

## 1. Why now

Fusion has crossed from toy to "moderately complex app that exhibits most of
what a Bevy desktop app needs": a main board, switchable views, a modal file
requester, a soon-wanted settings popup, transport control, and a growing pile
of keyboard shortcuts. The seams are already showing as ad-hoc accretion:

- **Every keyboard system hand-checks "is the requester open?"** (`arranger_edit_keys`,
  `transport_space_key`). That check does not scale: each new modal × each new
  shortcut system is a new place to get the guard wrong. We already hit the
  tension — Space-toggles-transport had to *drop* the guard to work while the
  requester is open, which now means Space won't type into a save-filename
  field. That's a symptom, not a bug to patch.
- **Transport/audio writes are scattered.** Play/Stop/seek/RTZ/load each poke
  `transport.*` (or the footer buttons, or a `TransportSeekRequest`) from
  wherever they happen to live. The "load a new MIDI leaves the old song
  playing" bug came straight from this: no single place owned "reset the
  transport", so the fix had to reach across `open_song` → footer-button
  trigger → seek message.
- **The settings popup** would be a *third* modal, each managed by hand.

Mark's directive: stop bolting on modals and shortcut-guards; design the
input/window/audio flow *once*, in a way that (a) scales to LOTS of shortcuts,
(b) lets ABP/MIDI/OSC fire the **same** actions as keyboard and mouse, (c)
doesn't block background work when a popup appears, and (d) is shared across
apps (fusion, fable) so the desktop coheres instead of each app reinventing it.

This is squarely the three RSI criteria: an Action registry is **legible**
(every shortcut and verb is queryable structured data, not buried in `match`
arms), **modifiable** (rebind by writing data, not editing code), and
**reconstructible** (an agent can enumerate an app's capabilities and drive
them).

## 2. The convergent finding

Three desktops, one answer. All three research streams — with no shared
prompt beyond the goal — arrived at the identical architecture:

> **A named, source-agnostic Action is the durable object. Every input source
> (keyboard, mouse, ABP, MIDI, OSC) is a symmetric *producer* of Actions.
> There is exactly one dispatch seam that resolves a raw event into either
> "forward to the focused surface" or "fire this Action". Bindings are data,
> not code. Modal capture is expressed as which systems are *allowed to run*,
> not as a flag every system re-checks.**

- **KDE/KGlobalAccel** proves the multi-source thesis: a keypress goes
  key→filter→daemon→**action**; a D-Bus `invokeShortcut "Window Maximize"` goes
  name→daemon→**same action**. They converge on one node. The registry sits
  *upstream of window focus* as a priority-ordered filter chain (consume/pass).
- **COSMIC/cosmic-comp** proves the config-as-data thesis: a shared
  `shortcuts::Action` enum is the contract between the compositor and the
  settings UI; bindings are layered RON (`defaults` + user `custom`),
  hot-reloaded through one `config_changed()` re-entry. smithay's
  filter-closure — *peek at the resolved key, return `Forward | Intercept(T)`
  where `T` is the compositor's Action type* — is the single most reusable
  primitive.
- **Bevy 0.19** gives us the idiomatic pieces to build it natively: a semantic
  `Action` enum + one `ActionRequest` **Message** as the spine; modal capture
  as **`States`/`SubStates` + `run_if(in_state(...))`** gating at *registration*
  (the flag-check moves into the scheduler, declared once); **`bevy_input_focus`**
  (`InputFocus` + bubbling `FocusedInput`) for which widget eats a keystroke;
  and one owning system per side-effect sink.

### What each got wrong (so we design it out)

- **Plasma's global-vs-local shortcut dualism** — two parallel systems for the
  same concept. Fix: one Action model with a `scope` *field*.
- **Plasma's `actionUnique` doubling as display string** — renaming for UX
  orphans a user's binding. Fix: **stable opaque id ≠ display label**, always.
- **Plasma's nullary, string-matched D-Bus** — `invokeShortcut "Expsoe"` fails
  silently; actions can't take a value. Fix: **typed `args_schema`** so MIDI
  CC → `mixer.gain set channel=3 value=0.7` works and `validate()`s at ingress.
- **Plasma has no notion of the source** — can't do per-source policy/audit.
  Fix: **tag every invocation with its origin** (kbd/mouse/amp/midi/osc). Cheap
  now, expensive to retrofit.
- **positional-INI persistence.** Fix: strict-data `.mix` (schema, comments,
  merge-sanity) — and it satisfies the mandate's "config through structured
  channels".

## 3. The proposed architecture

Five parts. Types are illustrative, not final.

### 3.1 The Action — the durable object

```rust
struct ActionDef {
    id: ActionId,          // stable opaque, e.g. "transport.toggle" — NEVER the label
    label: String,         // human display, freely rebrandable
    scope: Scope,          // Global | Window | Context(name) — a FIELD, not a subsystem
    args_schema: Schema,   // typed; nullary is just an empty schema
    default_bindings: Vec<Binding>,
}
```

An Action is **substrate state**, not a `match` arm. Per the substrate-first
service pattern, the registry lives in a SPEC-12 props namespace
(`actions.*`), and *invoking* one is a thin ABP verb. That is the honest
app-control port the control-plane ADR requires: an agent lists an app's
Actions by reading props, and fires one with a verb.

### 3.2 The dispatch seam — one funnel, `Forward | Dispatch`

Steal smithay's filter closure. One place converts a raw event into either
"the focused widget/app should see this raw" or "this is Action X, fire it":

```rust
enum Resolved { Forward, Dispatch(ActionRequest) }
fn resolve(event: RawInput, focus: &Focus, keymap: &Keymap) -> Resolved
```

The output is the spine message — **every** source writes it:

```rust
#[derive(Message, Clone)]
struct ActionRequest { action: ActionId, source: Source, value: ActionValue }
enum Source { Key, Mouse, Amp, Midi, Osc }
```

The keyboard translator, the ABP verb handler, the MIDI parser, the OSC
listener are all just `MessageWriter<ActionRequest>`. Handlers are
`MessageReader<ActionRequest>`. One join point; sources are symmetric and
trivially testable; `source` gives provenance for logging/undo/audit and the
RSI legibility criterion. **An ABP verb is indistinguishable from a keypress by
the time a handler sees it** — which is the whole point.

Same-frame collisions (a MIDI pad and a key firing the same Action) collapse in
the message bus. We already do exactly this: `on_seek_request` drains
`TransportSeekRequest` with `.last()` (latest wins). Generalise that discipline.

### 3.3 Modal capture — state-gated registration, not flags

```rust
enum UiMode { Board, Modal }              // a SubState; a ModalStack resource for nesting
app.add_systems(Update, board_shortcuts.run_if(in_state(UiMode::Board)));  // gated ONCE
app.add_systems(Update, (update_meters, apply_audio_intents, arranger_playhead)); // never gated
```

Opening a popup is `next_state.set(UiMode::Modal)`. Board input systems simply
don't run; audio, rendering, meters, transport keep running — because
"grabbing input" is expressed as *these translation systems are inactive*, not
*everything pauses*. No system body ever checks "is a popup open?" again. Which
*widget* eats a keystroke (text field vs board) is the finer grain, handled by
`bevy_input_focus` `InputFocus` + bubbling `FocusedInput` — this is the proper
fix for the Space-in-filename caveat: a focused text field consumes Space; with
nothing focused it reaches the transport Action.

Popups are **overlay UI layers, not separate winit windows** (one window, one
focus context, capture falls out of the state gate). Reserve real winit windows
for a later "tear-off a panel to a second monitor" feature only. Adopt KWin's
cooperative dismiss-on-outside-input model with an always-available escape;
never a hard grab that can wedge input.

### 3.4 The audio-intent broker — one sink for engine writes

Same shape, pointed at musicd:

```rust
#[derive(Message, Clone)]
enum AudioIntent { Play, Stop, Seek(f64), Load(SongRef), SetGain(Channel, f32), Reset, /* … */ }

// The ONLY system that touches the RT handle / revisioned prop writes / rings:
fn apply_audio_intents(mut intents: MessageReader<AudioIntent>, engine: Res<MixerHandle>) { … }
```

Nothing else writes to the engine. Payoff: one auditable choke point for the
revisioned-write / lock-free-ring discipline (today spread across five sites),
a natural ABP audio surface (an ABP verb is just another `AudioIntent` writer),
and record/replay for free. The "new-MIDI reset" bug becomes a single
`AudioIntent::Reset` instead of a cross-file dance. Keep `AudioIntent` a
*separate* type from `Action`: Actions are UI-semantic, AudioIntents are
engine-semantic; the map between them is where policy lives (Space →
Play-or-Stop depending on transport state).

### 3.5 Bindings as data

A serializable keymap (`.mix` strict-data), layered defaults + per-app user
`custom`, hot-reloaded — COSMIC's model, our format. One system translates
chords → `ActionRequest`. The same Action ids are what MIDI/OSC/ABP reference,
so **one table describes every trigger surface**. Rebindable at runtime,
agent-writable (the "modifiable by agents" criterion, free).

## 4. Where it lives — crates and the mesh

- **`cosmix-actions` (new, core-first crate).** The `ActionId`/`ActionDef`/
  `Binding`/`Keymap` types + the `resolve()` logic + `.mix` (de)serialisation.
  Pure, mesh-free, unit-testable (the core-and-citizen pattern); ABP/props
  integration behind the `cosmix` feature. This is the shared contract fable and
  fusion both depend on — the thing that makes the desktop cohere.
- **Per-app wiring** (in each app's Bevy crate): the input translators, the
  `UiMode` state, the `apply_audio_intents`/side-effect sinks, the app's own
  `ActionDef` set.
- **The mesh surface** (substrate-first): the app publishes its Action registry
  into an `actions.*` props namespace; a thin `action.invoke id=… args=…` ABP
  verb fires one. This is ABP-as-D-Bus, but **typed** — `args_schema` +
  `validate()` at ingress, which Plasma never had. Cross-app window verbs
  (`activate`, `close`, `move-to-workspace`) mirror cosmic-protocols'
  Toplevel-Management, ABP-native.

Focus & authorization must be **explicit broker state** — Cosmix has no Wayland
per-client focus model to lean on. Who may emit an Action, and whose wins on a
same-frame tie, is our code, not the compositor's. Two-tier trust (trusted WG
peers get direct verb access; untrusted goes through a mediating boundary)
matches the existing `amp_security_model`.

## 5. What this fixes, concretely

| Today's ad-hoc thing | Under the broker |
|---|---|
| Every kbd system checks `requester.is_open()` | `run_if(in_state(Board))`, declared once |
| Space can't both toggle transport *and* type in a filename | `InputFocus`: focused text field consumes Space; else it's the Action |
| Settings popup = a third hand-managed modal | A `UiMode::Modal` overlay + its `ActionDef`s; zero new guards |
| "New MIDI leaves old song playing" cross-file fix | `AudioIntent::Reset` at one sink |
| Scattered `transport.*` writes | One `apply_audio_intents` owner |
| A giant shortcut `match` | A `.mix` keymap table |
| No way for ABP/MIDI/OSC to drive the app | They're `ActionRequest` producers — same as a key |

## 6. Migration is incremental, and the codebase already leans this way

This is not a rewrite. Fusion **already has proto-brokers**: `on_seek_request`
is a single-owner, last-wins message drain; the ctk `MixerBinding`→`Activate`
observer path is an action-indirection; `FileRequesterState.is_open()` is a
proto modal-state. The design generalises patterns already present, so each
phase is a local refactor with the app working throughout.

## 7. Phased plan (recommended)

- **Phase 0 — Fusion internal spine (small, high-value, do first).** Introduce
  `Action` + `ActionRequest` + `AudioIntent` *inside fusion* (no new crate yet).
  Migrate transport writes → `AudioIntent`; add `UiMode` SubState + gate the
  keyboard systems (deletes the `is_open()` guards); route Space through an
  Action. Fixes the flag-check smell and the audio-write scatter with types we
  already understand. Ships as ordinary fusion work.
- **Phase 1 — Extract `cosmix-actions` + data bindings + focus.** Lift the types
  into the shared crate; move the keymap to `.mix` (layered defaults/custom in
  `~/.fusion`); adopt `bevy_input_focus` for text fields (properly fixes the
  Space-in-filename caveat). **Build the settings popup as the first
  `UiMode::Modal` consumer** — the deferred ask lands here, on the foundation.
- **Phase 2 — ABP surface.** Publish `actions.*` props; thin `action.invoke`
  verb; source-tagging end to end. Now an agent/Mix script drives fusion — the
  control-plane ADR's app-control port, realised.
- **Phase 3 — Second producer + second app.** MIDI/OSC producers into the same
  bus; **fable adopts `cosmix-actions`**. Two apps, one input/action model —
  the desktop foundation.

Trial the loop on Phase 0's ~3 systems before fanning out (the standing
"trial-run before fan-out" discipline).

## 8. Decision points for Mark (this is why it's PROPOSED)

1. **Shape approval.** Is the semantic-Action + one-message-bus + state-gated-
   modal + audio-intent-sink architecture the direction? (My recommendation:
   yes — three independent desktops converged on it, and it's the honest
   implementation of the app-control port you already locked in.)
2. **Physical-input layer: hand-roll vs adopt a crate.** For the *keyboard→Action*
   translation only, we can hand-roll (zero deps, ~1 system) or adopt
   `bevy_enhanced_input` (0.26, targets 0.19) — push/observer model, built-in
   *mocking* (external triggers as a first-class path, not a hack), context
   priority for modals; it's the sanctioned future Bevy input abstraction.
   **My lean: hand-roll the translator in Phase 0, keep the `Action` enum as
   the stable spine, adopt `bevy_enhanced_input` at Phase 1 if the physical
   layer earns it.** The spine doesn't change either way.
3. **`cosmix-actions` crate now or after Phase 0?** My lean: after — prove the
   spine inside fusion first, extract once fable is ready to consume it, so the
   shared API is shaped by two real users, not one guess.
4. **Scope of the first cut.** Do you want Phase 0 only (fix the smells, no new
   surface), or Phase 0+1 (settings popup + data bindings) as one arc?

I will not start the refactor until you've reacted — this is foundational and
cross-app. The concrete Fusion bug-fixes from this session (ruler-drag, knob,
reset, requester size) are independent and already built; they don't wait on
this.

---

## 9. Codex review of the Phase-0 scaffold (2026-07-20) — verdict: HELD, not converged

Cold adversarial review (codex `gpt-5.6-sol`, thread
`019f7f85-a623-7812-8cb5-c52d98ae5d94`), run against the live `$COSMIX` tree over
the committed Phase-0 scaffold (`gui/apps/fusion/src/action.rs` + wiring in
`main.rs`/`file_io.rs`/`views.rs`). Six findings (5 MAJOR, 1 MINOR, **no
BLOCKERs**). Claude verified the falsifiable ones against primary source before
recording. **No fixes applied** — Mark chose to hold for the §8 shape decision,
because the honest fixes (ordered system sets, broker-owned desired-state,
routing footer/ruler through the broker) *are* the §8 direction call, not
patches.

Bottom line: the narrow happy path works (one Space → one `ActionRequest` → one
`AudioIntent::Toggle` → one activation), but Phase 0 as written does **not**
deliver its three headline claims — "one sink", "same-frame modal capture", and
"symmetric multi-producer". The comments/ADR over-state what the code does.

| # | Sev | Verified | Gap |
|---|---|---|---|
| 1 | MAJOR | ✅ confirmed | **"One sink" is false.** `AudioIntent::Seek` and `::Stop` are constructed nowhere (dead variants). Ruler seeks (`views.rs:1756/1787`) write `TransportSeekRequest` directly, bypassing `apply_audio_intents`; CTK footer RTZ/Play/Stop/scrubber also write transport bindings directly (`ctk/src/mixer.rs:2896/2905/2917/2930`). Either route them through the broker, or redefine CTK's pipeline (not `apply_audio_intents`) as the real sink and weaken the claim. |
| 2 | MAJOR | ✅ confirmed | **`.chain()` orders only the 4 in-plugin systems.** `apply_file_results` (Reset) and `autoplay` (Play) have no ordering edge vs `apply_audio_intents` → nondeterministic **one-frame** latency (new bank runs at old playhead/state — the exact bleed-through Reset claims to prevent). Not message loss (Bevy double-buffer delivers next frame), latency. Fix: ordered `ActionProduce → ActionRoute → ActionApply` system sets. |
| 3 | MAJOR | ✅ confirmed | **`UiMode` lags modal-open by a frame** (`NextState` applies in `StateTransition` after `PreUpdate`). Proof it isn't delivered: `arranger_edit_keys` is gated on `in_state(Board)` *and still* keeps a body-level `if requester.is_open() { return }` (`views.rs:1187`) — which directly disproves the "no system body checks is-a-popup-open" comment (`action.rs:81`). Also: a future settings popup setting `UiMode::Modal` gets stomped back to `Board` by `sync_ui_mode` whenever the file requester closes. Fix: real shared modal-ownership state set before `StateTransition`. |
| 4 | MAJOR | plausible | **Toggle reads acknowledged state, not last-issued.** `transport_is_playing(&state)` at apply-time is correct only with ≤1 outstanding intent. Double-Space before the Play ack → both read stopped → both Play (expected: stopped). Two Toggles same frame pick the same target. Fix: broker-owned desired-transport-state folding the ordered intent stream. |
| 5 | MAJOR | plausible | **`intents.clear()` (pre-footer) permanently drops intents;** Reset's seek/stop also dropped downstream during an active scrub or mixer-not-ready. Load during reconnect/scrub → Stop/seek-zero vanish, playback continues at old state. Fix: retain pending intents until prereqs exist; make Reset a barrier that supersedes a scrub. |
| 6 | MINOR | ✅ confirmed | **Provenance is decorative.** `route_actions` drops `req.source`; `AudioIntent` has no field to carry it. Clippy flags `source` as never-read. Cheap to fix now, cross-surface migration once audit/undo + external producers exist. |

**Most-dangerous unenforced assumption (codex):** *at most one outstanding
transport intent, every producer runs before the sink, the footer exists, and
mixer state already reflects the prior write.* A second keyboard/ABP/MIDI
producer silently breaks all four — nothing in system sets, types, or module
visibility enforces it.

**Gate check:** `cargo check` passes; `cargo clippy --all-targets -D warnings`
**fails** (unused `source`, never-constructed `Stop`/`Seek`, plus pre-existing
fusion lints). Working tree clean.

**Artifact to discount:** codex reported "the ADR doesn't exist" — it searched
`$COSMIX`; this ADR lives in `$CMCTL` (private hub), cross-repo and expected. It
reviewed the code's own comments, which is what matters.

**How this feeds §8:** findings #1/#2/#4 argue the spine needs ordered system
sets + a single-writer (broker-owned desired-state) discipline *baked in before*
it spreads to a second app — a design commitment, i.e. decision #1 (shape
approval) and #4 (scope of the first cut). The scaffold stays as exploration
residue until Mark answers §8; the fixes then fold into whichever direction is
chosen.

## 10. §8 resolution — converged 2026-07-20 (awaiting Mark's ratification)

Mark leaned toward Claude's recommendations and directed a codex-review loop
"until resolved". Three rounds on codex thread `019f7f85` (round 1 = the §9
scaffold review; rounds 2–3 = adversarial review of the resolution itself), every
checkable claim verified against source. **The loop converged**: codex's verdict
on the revised shape is *"the revised direction is sound… the remaining problems
are contract precision, not a return to the old architecture."* The residuals are
not open shape questions — they are the **Phase-0 acceptance criteria** (§10.6).

### 10.1 The one reframe that reshaped everything — transport ownership

The scaffold's "`apply_audio_intents` is THE sink" is false and can't be rescued
by a fusion-tier desired-state reducer: **CTK already owns the transport command
lifecycle** (per-path in-flight exclusion + gesture ownership, CAS revisions +
retry, latest-wins outbox, authoritative reply reconciliation at `mixer.rs:1523`,
epoch fencing). A fusion reducer above it would be a *weaker duplicate racing the
footer's direct writes* (`on_control_change` mixer.rs:697 vs `on_seek_request`
mixer.rs:470 — two entry authorities into one pipeline).

**Honest architecture: "Fusion action *ingress* above a CTK-owned versioned
transport lifecycle."** Not "action broker owns the engine". Fusion keeps action
routing, provenance, app-level policy; CTK owns desired→issued→acknowledged→
applied→superseded. Raw transport writes become CTK-private so fusion *cannot*
bypass the reduction point.

### 10.2 Q1 — Shape: APPROVE, conditional on the lifecycle contract

Named-Action + one `ActionRequest` bus **as ingress** + state-gated modal +
routing into the CTK reducer. The bus is ingress to a durable command processor,
never itself the state machine (that was finding #4's root — not an indictment of
message buses, but of treating the reader as the complete state machine).
**Caveat (codex, verified):** CTK's existing machinery is a sufficient *foundation*
but not a finished lifecycle — `queued_latest` stores only `path→value` (source/
generation/correlation lost at mixer.rs:352/402); reconnect and epoch reset clear
queued+in-flight work (mixer.rs:1087/1464). So this is a **real internal CTK
data-model refactor** (command envelopes surviving queue→issue→ack→supersession),
not merely exposing an existing reducer.

### 10.3 Q2 — Physical input: HAND-ROLL now

Hand-roll the keyboard→Action translator; `bevy_enhanced_input` does **not** fix
the modal race (its contexts evaluate in `PreUpdate`; the requester doesn't
activate until `receive_requests` in `Update` — same race, different abstraction).
Revisit it at Phase 1 for context layering/consumption/chords only. **Amendment:**
modal-capture authority is **not** a bare `UiMode::Board|Modal` enum — it is an
**owner-token registry** (`captured = !owners.is_empty()`) or a real stack, so
nested/queued modals can't overwrite each other; `UiMode` becomes a *projection*
of that authority, not independently-writable state. Capture is acquired when a
request is **accepted or queued**, released after the final close latch.

### 10.4 Q3 — `cosmix-actions` extraction: FULLY RESOLVED

Not "wait for fable" (fable is *already* a diverged second translator —
`browser_keyboard` at browser.rs:1871 uses `InputFocus`+`InteractionState`).
Sequence: (1) fix the Phase-0 contract in fusion; (2) immediately port **two
representative fable actions** (one global, one modal-sensitive); (3) extract
**only what survives both apps**; (4) then Phase 1. Likely-stable subset is small:
a generic action envelope + provenance/correlation + producer/router stage
contract. Do **not** extract `Action`/`AudioIntent`/`UiMode`/the reducer, nor even
`Source` yet (missing internal/widget/automation cases).

### 10.5 Q4 — First cut: Phase 0 only, but it is *foundation hardening* (Fusion + CTK)

Not a small scaffold. "Done" requires a **real modal-capture fix** — the two
disciplines (ordered sets, CTK lifecycle) do *not* fix the frame-lag. The
body-level `is_open()` guard at views.rs:1187 is removed **last**, only after the
tests below pass **and every board-input system is inventoried into the gated
set** — deleting it as a gesture before then reproduces the bug.

### 10.6 Phase-0 acceptance criteria (the converged "done" — residuals from all 3 rounds)

**Continuous-control boundary (enforced by type, not caller judgement):** a
gesture lifecycle `BeginGesture / UpdateGesture / CommitGesture / CancelGesture`
— only `UpdateGesture` is lossy/latest-wins; commit/cancel are durable. Ruler
*click* stays a discrete `ActionRequest` (views.rs:1747); ruler *drag* (views.rs:1778)
bypasses the Action bus but **still carries** a CTK command envelope with source +
gesture-id + generation. No metadata-less path to the reducer.

**Arbitration rule made explicit** (today CTK silently discards an app seek during
a scrub, mixer.rs:479): a discrete seek during a gesture must be *rejected /
queued / cancels-the-gesture / supersedes* — a stated, tested outcome, never
silent disappearance.

**Modal tests:** open requester + Ctrl+Z same tick → no undo; Escape-close → no
Escape leak; queued/nested ownership stays captured; `just_closed` close-frame
latch (file_requester.rs:189) preserved.

**Transport-lifecycle tests (the actual central deliverable — don't let modal
tests stand in for it):** two toggles before first ack → correct net desired
state; a newer command can't be reversed by an older command's ack; discrete
Reset during a scrub has a tested outcome; disconnect/epoch reset has an explicit
desired-command policy (retain/reject-with-outcome/supersede, never vanish);
composite Reset reports completion only under the defined stop-plus-seek rule; a
producer in `ActionProduce` is routed and submitted **same frame**.

**Gate:** clippy green (budget the pre-existing unrelated fusion lints honestly).

### 10.7 Biggest residual risk — the word "applied" (verified, mixer.rs:1387, mixer_host.rs:443)

musicd **coalesces `SetControls` to the newest snapshot** while advancing the
high-water revision across *every* drained command. So `applied.revision >=
command_revision` can mark an older command "applied" whose values **never
individually reached the DSP** — the newest snapshot won. The lifecycle must name
this state `CoveredByAppliedRevision` (distinct from exact application), **or** the
engine protocol is extended for per-command observability. Naming five states
doesn't solve the observability gap; until fixed, the lifecycle can confidently
report something the engine never did. **Documented constraint, not a blocker.**

### 10.8 Decision left for Mark

The technical shape has converged; ratification is yours (foundational, cross-app,
touches CTK). **Recommendation: ratify §10 as the resolution**, flip status to
ACCEPTED, and schedule the Phase-0 rebuild against §10.6 as the next arc — or
keep it queued behind the current Fusion piano-roll work. No code changes until
you ratify.

---

## 11. Phase 0 shipped — 2026-07-21 (record + honest deferrals)

All four stages implemented via the ultracode loop (frontier-Claude drives, codex
designs+implements at stages, Claude verifies against the live harness, a **fresh**
codex thread cold-reviews each diff, convergence before commit). Every stage's cold
review found a real MAJOR; none shipped on first-pass output. Plan file
`_plan/2026-07-21-action-broker-phase-0-rebuild.md` retired (shipped).

| Stage | Commit | What it delivered | Cold-review MAJOR (fixed) |
|---|---|---|---|
| A | ctk 0.11.1 | CTK-owned versioned transport command lifecycle: internal `CommandMeta` envelope (WriteRequest stays wire-pure), owner-scoped gestures, shared `submit_write`, observable `TransportCommandOutcome` (Superseded/Rejected/phase-tagged Abandoned/`CoveredByAppliedRevision`/CoverageUnknown), bounded local-issue backoff→Rejected | ① path- not owner-scoped gestures ② non-terminating requeue |
| B | ctk 0.11.2, fusion 0.1.12 | Ruler drag = one owned transport.position gesture (`TransportSeekGesture` Begin/Update/Commit/Cancel); tightened position collision to owner comparison; click stays discrete | Pointer<Cancel> routes to hovered entity → ownership leak (global cancel observer) |
| C | ctk 0.11.3, fusion 0.1.13 | Race-free single-authority modal capture: `FileRequesterSystems` ordering contract + `BoardInputSystems` run-condition; UiMode deleted; `arranger_input` kept as continuous-input boundary | In-flight board gestures committed under the modal (hybrid central-cancel + per-handler self-abort) |
| D | ctk 0.11.4, fusion 0.1.14 | Ordered `ActionProduce→Route→Apply` + Reset dominance (closes finding #2 one-frame bleed); narrow CTK `MixerTransportIngressSystems` ingress ordering | (design consult found the Reset-dominance flaw pre-impl; cold review found only 1 MINOR) |

Also: deflaked a pre-existing Stage-A wall-clock test race (`persistent_local_issue_failure`).

**Claims Phase 0 can now truthfully make** (the three the original scaffold overstated):
one CTK-owned transport sink (not scattered writes); same-frame modal capture (not a
lagging UiMode projection); symmetric producers through an ordered ingress (not
unordered intents). Verified: ctk 112 tests, fusion 20 tests, clippy clean on ctk,
fable builds throughout.

**Deliberately DEFERRED (honest — not done):**
- **Owner-token modal registry** (ratified §10.5's general form) → Phase 1, when Fusion
  gains a *second* modal source (settings popup). Today one source (file requester);
  the registry would be speculative.
- **`bevy_enhanced_input`** → Phase 1 if the physical layer earns it (doesn't fix the
  modal race). Spine unchanged either way.
- **`cosmix-actions` crate extraction** → after porting two representative Fable actions
  (Fable already diverged); extract only what survives both apps.
- **End-to-end Source provenance** → `ActionRequest.source` is ingress-only, dropped by
  `route_actions`; `AudioIntent` carries no source. No Option<Source> half-envelope added.
- **Atomic RT bank+stop+seek** → ordering removes the deterministic extra-frame delay but
  `ship_bank` still precedes Reset apply; a true zero-block needs an RT-side "install bank
  stopped at zero" op (§10.7-adjacent).
- **Two-toggles-same-frame policy** (original finding #4) → unchanged; system sets don't
  repair it (both toggles read the same acknowledged state). Needs the desired-state
  reducer to own it — separate work.
- **`TransportCommandOutcome` consumer** → none yet (no operation acts on it; AudioIntent
  has no correlation id to associate an outcome). Produced + tested only.
- **Touch multi-pointer / no-hover cancel** (ruler) → single-pointer desktop is leak-safe;
  pointer-id-aware lifecycle is Phase 1.
- **Autoplay-swallow race** → effectively unreachable today (documented on `autoplay`);
  revisit if an automatic startup file-load is added.

Phase 1 entry points, in likely order: settings popup → owner-token registry + a real
second modal source; then `cosmix-actions` extraction once Fable ports two actions.

---

## 12. Phase 1, arc 1 shipped — 2026-07-21 (owner-token modal registry + settings popup)

The ratified §10.5 general owner-token registry, deferred in Phase 0 until Fusion had
a second modal source. Two reviewable changes, same discipline (codex designs/implements,
Claude verifies on the harness, fresh-thread cold review, converge before commit).

- **Change 1 — ctk 0.12.0:** shared layer-aware `ModalCapture` authority
  (`gui/ctk/src/modal_capture.rs`): tokens, layers (requester 1000, interaction 1100),
  `acquire`/`release_latched`(retained through `Last`)/`is_captured`/`is_top`/`top_owner`;
  top = `max(layer, acquisition_serial)` so keyboard routing agrees with visual Z.
  `FileRequesterState` + `InteractionState` migrated to acquire/release tokens and gate
  their keyboard on `is_top` (one Escape can't close two stacked modals); `just_closed`
  and both `is_open()` removed (breaking → ctk MINOR). All fusion/fable capture-gates
  repointed to `ModalCapture` (`is_captured` for board/pointer/wheel, `is_top` for service
  keyboards). Cold review: **zero defects**.
- **Change 2 — fusion 0.1.15:** Fusion `Settings` modal (`settings.rs`, layer 900) as the
  real second owner — File-menu entry, one honest setting (session SoundFont path +
  "Choose…" reusing the existing flow) + Close, styled to match the requester,
  `TabGroup::modal()`. Multi-owner arbitration proven by test: settings(900) → nested
  requester(1000) on top → requester Escape closes it (stays top through its close frame)
  → settings survives and is top again after `Last`.
- **Supersession (ADR §5 realised):** the cold review surfaced that Phase-0's ungated
  "global Space" was a placeholder that double-acted with a focusable modal button.
  `keyboard_actions` is now gated on `!ModalCapture::is_captured()` — a modal owns the
  keyboard while captured. Finer focused-widget consumption (a text field *outside* a
  modal) remains the later `bevy_input_focus` refinement. The Phase-0 test was reversed
  (`space_transport_toggle_is_suppressed_while_modal_captures`).

**Deferred within arc 1** (documented in code): production menu-invocation focus
restoration — the menu click clears focus before `MenuActivated`, so `previous_focus`
records `None`; the fix (carry an invocation-focus entity in the menu event) belongs to
the Phase-1 focus arc.

**Arc 2 shipped — ctk 0.13.0 / fusion 0.1.16 (2026-07-21):** focus-aware Space +
menu invocation-focus restoration, finishing the §5 focus model for the *earned*
cases. A scope pressure-test (codex) corrected an initial wrong read of mine — CTK
widget bundles give board controls `TabIndex`, so a focused Mute/RTZ + Space
double-acted (widget + transport). Fix: a `FocusedInput<KeyboardInput>` observer
emits a buffered toggle only when Space bubbles to the Window unconsumed; focused
button/checkbox/`EditableText` consume it first. `keyboard_actions` (still
`ActionProduce`, `is_captured`-gated) reads the buffer → Stage-D ordering intact.
Menu now carries `MenuActivated.invocation_focus`; Settings + FileRequester restore
it on close (before validation, so a rejected action doesn't strand focus).
`bevy_enhanced_input` confirmed *not* earned — native Bevy focus is the primitive.
Cold review: mechanism clean, 3 MINORs (2 fixed, 1 = the deferred finding #4).
*Deferred:* full non-modal board keyboard traversal (`TabGroup`) — a deliberate a11y
cut; two-toggles-same-frame (finding #4) — needs the transport desired-state reducer.

### Arc 3 — `cosmix-actions` extraction: DEFERRED (gate not met), Phase 1 closed at its earned boundary (2026-07-21)

A viability consult (codex, verified vs source) found the extraction precondition
("extract only what survives both apps") **is not met** — and corrected a framing
error: Fable is not a diverged *consumer* of the action pattern; it never adopted it.

- **Fable** (`browser_keyboard`, browser.rs:1870) is a direct imperative translator
  (raw `ButtonInput` → mutate `BrowserState`); no `ActionRequest`/`Action`/intent bus.
- **Fusion's own "action bus" is a narrow transport-policy spine**, not a general
  model: `Action` has one variant (`TransportToggle`, action.rs:33); the sole
  `ActionRequest` writer is the Space translator; autoplay + file-reset enter directly
  at `AudioIntent` (main.rs:54, file_io.rs:298); undo/redo + arranger-edit stay
  imperative behind `BoardInputSystems`; pointer Play/Stop goes through CTK. So there
  is no mature shared action model for Fable to join.
- Nothing is a stable generic contract worth extracting now: `Source` is speculative
  (4/5 variants unused, provenance discarded at routing), the producer/router/apply
  sets are Fusion scheduling policy (tied to file-results + reset-dominance + CTK
  ingress), `board_input_enabled` is a 2-line run-condition over the already-shared
  `ModalCapture`, and focus-consumption is per-app policy (Fusion: `FocusedInput`
  propagation; Fable: inline `editable.contains`). Forcing Fable onto `ActionRequest`
  would be churn and would not deliver agent/ABP-operability (neither app enables CTK's
  `amp` feature; there's no correlation-id/authorization/result/audit contract).

**Revisit extraction only when a second app independently has:** semantic command
ingress from ≥2 real sources, the same provenance + scheduling/reduction + observable
completion needs, and enough duplicated implementation that moving it *removes* code
rather than creating adapters.

**Phase 1 — complete at its earned boundary.** Shipped: the shared `ModalCapture`
ownership registry (arc 1) and Fusion's focus-aware Space + menu invocation-focus
restoration (arc 2). Stated precisely — the *cross-app* focus model is NOT "done":
Fable still uses its own partial focus predicate; only Fusion's focus wins shipped.

**App-local follow-ups (NOT shared-crate evidence, tracked separately):** (a) **SHIPPED
2026-07-21 (fable 0.3.2, §13).** Fable's `browser_keyboard` only recognised *editable*
focus, so a focused non-editable toolbar control didn't own its keys — now fixed. (b)
Fable toolbar Back/Parent/ToggleHidden run a semantic `ToolbarAction` handler
(browser.rs:1525) while the shortcuts repeat the ops directly (browser.rs:1920) —
app-local dedup, still open.

Plans retired (arcs 1 & 2 shipped; arc 3 deferred).

---

## 13. App-local follow-up (a) shipped — Fable focused-action key ownership (2026-07-21)

fable 0.3.2 ($COSMIX `e2efbec`). Closes follow-up (a) above. **Not** action-broker
architecture — deliberately app-local, per the arc-3 finding that Fable is an
imperative translator with no action bus to join.

**What shipped:** `browser_keyboard` (browser.rs:1870) now, after its PathInput and
editable-focus handling, treats a focused non-editable action control (toolbar / place /
sort button — each carrying `ToolbarAction` / `PlaceAction` / `SortAction`) as owning its
keys. A single Enter-or-Space press fires the *same* `Activate` observer a mouse click
uses (via `on_nested_action_click`), exactly once even on a same-frame Enter+Space; every
other key is swallowed by an unconditional `return` so nav hotkeys (arrows / Backspace /
Home / F6 / Ctrl+H / Alt+arrows) no longer leak to the global handlers while an action is
focused.

**The one real design fork (codex design consult, 2 rounds):** the *idiomatic* Bevy fix
is to swap these buttons to `bevy::ui_widgets::Button` (which ships an Enter/Space→`Activate`
keyboard observer) — **rejected**. Fable uses the legacy `bevy::prelude::Button` marker
everywhere and hand-rolls activation; introducing the modern widget makes pointer
activation depend on `button_on_pointer_click`'s `Pressed` timing, which is unreliable
(the exact trap ctk documents and worked around with `ActivateOnPress`,
file_requester.rs:601). Keeping the legacy marker + adding an app-local keyboard bridge
gives exactly one `Activate` trigger per input path (no double-fire, no race) and keeps
fable internally consistent. Scope stops short of board-wide Tab traversal: fable has no
non-modal `TabGroup`, so these controls are reachable only by mouse/programmatic focus
today — the `TabGroup` traversal cut remains a separate, deliberate accessibility pass.

**Verify + review:** build + test + clippy green (5 new unit tests: Enter/Space activate
once; non-action focus and modal-capture do not; a focused action swallows F6 with the
active pane unchanged — the last guards the swallow `return` against regressing into the
Enter/Space guard). Cold-reviewed on a fresh codex thread: double-activation, placement
(PathInput entities carry `EditableText` so they return before the action branch),
modal precedence, and test fidelity all dispositioned; its one MINOR — swallow behaviour
untested — is the F6 test.

**Adjacent pointer defect found, deferred:** when an action entity is *itself* the click
target (bare button padding, not an icon/text child), `on_nested_action_click`
(browser.rs:~1618) closes the context menu and returns *without* triggering `Activate` —
a narrow mouse dead-zone, separate from this keyboard cut. Track with follow-up (b).

---

## 14. App-local follow-up (b) shipped — Fable nav dedup + context-menu click fix (2026-07-21)

fable 0.3.3 ($COSMIX `70f307c`). Closes follow-up (b). Two app-local cleanups, each
proven with a failing-then-passing test (wrote the test against current code, watched
it fail, then fixed).

**Part A — nav dedup.** Parent and ToggleHidden were duplicated between the toolbar
`Activate` handler (`on_toolbar_action`) and the keyboard shortcuts (Backspace, Ctrl+H).
Extracted `navigate_parent` + `toggle_hidden` as the single source of truth and routed
both call sites of each through them. Back/Forward/Home/Refresh were already single
shared-fn calls, left as-is. **Rejected a `NavCommand` enum** (codex design consult):
Back/Forward already centralise in `navigate_back`/`navigate_forward`, Home/Refresh are
trivial non-duplicated calls, so an enum + dispatch match would be an adapter over
already-shared functions — churn, not consolidation. Two domain-named helpers are the
honest minimal cut.

**Part B — `on_nested_action_click`, two real bugs, both proven by test.** This is the
ONLY mouse `Activate` path (fable uses the legacy `bevy_ui::widget::Button` marker, not
`ui_widgets::Button`, so no bevy pointer observer fires — verified vs source twice, and
this is what makes the fix double-fire-free).
1. **Padding dead-zone:** a primary click whose original target was the action entity
   itself (bare padding, not an icon/text child) hit an early
   `if actions.contains(entity) { close_context_menu; return; }` that returned WITHOUT
   activating → the button did nothing. Deleted the early block; the loop's first
   iteration handles the direct target uniformly. Test: `[]` → `[true]`.
2. **Activate-after-despawn ordering (the notable find):** a popup Copy/Move/Delete
   entity is a *child* of the context-menu entity, and `close_context_menu` does
   `commands.entity(menu).despawn()` (recursive in Bevy 0.19). The old order was
   `close_context_menu` THEN `commands.trigger(Activate)`; commands apply FIFO, so the
   popup entity was despawned before the `Activate` observer ran, `on_toolbar_action`'s
   `actions.get(entity)` returned `Err`, and **the op silently no-op'd — right-click
   Copy/Move/Delete had been dead.** Reordered to trigger `Activate` before closing the
   menu. `close_context_menu`'s `.take()` clears the slot synchronously, so the later
   `close` in `on_toolbar_action` sees `None` — exactly one despawn, no panic. Test:
   `[false]` (activated, but component already gone) → `[true]`.

**Codex thread correction worth recording:** the fresh-thread cold-review consult
initially claimed deleting the early block would double-activate because "these are also
Bevy `Button`s". That conflated `bevy_ui::widget::Button` (legacy marker, no observers)
with `bevy_ui_widgets::Button` (headless widget, has observers). Refuted with source
(browser.rs:23/28 imports, ctk prelude re-exports no bare `Button`); codex conceded and
converged on delete-early-block + reorder. The legacy-marker fact is load-bearing across
both §13 and §14 — fable's actions have exactly one activation path, the app's own.

**Verify + review:** build + test + clippy green (21 tests incl. 2 new pointer tests via
a faithful `Pointer<Click>` harness). Cold-reviewed on a fresh codex thread: no
BLOCKER/MAJOR/MINOR; the one NIT (the test pointer used the clicked entity as its own
render-target window, a theoretical propagation-loop) fixed by spawning a distinct
`Window` entity. Follow-ups (a) and (b) both now shipped; the remaining fable items
(#3 transport reducer, #4 board `TabGroup`) are unrelated to this ADR's arc.

---

## 15. Finding #4 closed — transport desired-state projection (2026-07-21)

ctk 0.14.0 + fusion 0.1.17 ($COSMIX `ca1d023`). Closes the finding-#4 deferral
recorded at §11 / action.rs — rapid transport toggles resolving against the laggy
acknowledged `MusicdMixerState`, so two toggles before the engine acknowledged the
first (same-frame or cross-frame) both read stale state and resolved to the same
target (two Plays instead of play-then-stop).

**Ownership decision (Mark, over the handoff's assumed fusion-local shadow).** The
handoff planned a bounded fusion `pending: Option<bool>` shadow. The design consult
(codex, verified vs source) found fusion **cannot** reconstruct the effective desired
state from ctk's public API: the real desired value lives in `MixerIo`'s **private**
`queued_latest → pending → retries` precedence (the same one `seed_gesture_baseline`
uses at mixer.rs:1449); `revision()`/`last_applied_revision`/`TransportCommandOutcome`
are all insufficient. A fusion shadow would be a second, weaker, drift-prone tracker.
So: **CTK owns transport state and exposes a read-only projection of what it is already
driving toward; fusion owns only the toggle/fold policy.** This is a small ctk public
API addition — a scope step up from "bounded fusion-local", surfaced to Mark and
ratified because it's a public-API-shape call.

**What shipped.**
- *CTK 0.14.0 (new public API):* `TransportState` SystemParam with
  `desired() -> Option<DesiredTransport>` / `desired_playing() -> Option<bool>`, where
  `DesiredTransport { playing, provisional }` and `provisional` = the value comes from a
  queued/in-flight/retrying write (not the acknowledged store). `None` unless
  Connected+ready. The `queued→pending→retry→acknowledged` precedence was factored out of
  `seed_gesture_baseline` into a shared `newest_desired_write` (gesture baseline behaviour
  preserved). `MixerIo` stays private; only the narrow projection is public.
- *Fusion 0.1.17:* `apply_audio_intents` folds one frame's ordered intents to at most one
  Play/Stop trigger, resolving Toggle against the projection's provisional baseline (so
  cross-frame toggles order correctly). Reset stays dominant (unconditional stop + rewind).

**The convergence catch (why the re-review is load-bearing).** The first cut suppressed a
write whenever the folded target equalled the projection — **including a provisional
in-flight value**. A fresh-thread cold review found the MAJOR: if the in-flight write is
later CAS-rejected / retry-exhausted, the suppressed reaffirmation is lost and the net
intent is stranded (three toggles from stopped → stays stopped). "Suppress against
acknowledged instead" is *also* wrong — it breaks the ordinary cross-frame toggle. The fix
is **provenance**: suppress only against a *settled* (acknowledged) value; equal-to-
*provisional* **reaffirms** (re-queues behind the in-flight write via `queued_latest`, so a
rejection re-drives rather than strands). A `saw_state_intent` guard keeps seek-only/empty
frames from reaffirming. Re-review verdict: fully fixed, no issues.

**Verified independently:** ctk `cargo test --features mixer` 127 pass + clippy `-D warnings`
clean; fusion 41 pass, including two real-lifecycle integration tests — cross-frame toggle
resolves against provisional playing, and a reaffirm survives a CAS rejection (the exact
MAJOR scenario).

**Still deferred (unchanged):** RT-atomic bank+stop+seek; Reset-during-scrub gesture
collision; and the not-ready/pre-footer Reset drop (finding #5) — a reaffirmation, like any
write, can still receive a terminal transport failure; that general write-failure-recovery
contract is separate from this toggle fix.

---

## 16. Board `TabGroup` keyboard traversal — DEFERRED after a design pass (2026-07-21)

Handoff item #4. Design consult run (codex, verified vs bevy 0.19 source); **owner
decided to defer**. No code shipped — this section is the warm-start record.

**Why deferred (the earned-ness call).** Board Tab traversal advances none of the
three agent-first criteria: agents drive fusion through ABP / app-control ports with
stable widget identities, not Tab keys. It is a *human* accessibility feature, which
the mandate ranks below agent-usability. And even done correctly it is weak human UX
in raw form — the Mixer alone is **~162 tab stops** (5 controls × 32 strips + master);
a linear walk needs a *designed* grouped/directional scheme, not sequential stops.

**Why it is NOT the bounded cut the handoff implied.** bevy's `gather_focusable`
(bevy_input_focus 0.19 tab_navigation.rs:323) collects every `TabIndex >= 0` entity
under a non-modal `TabGroup` and does **not** filter `Display::None` / `Visibility` /
`InteractionDisabled` — and offers no hook. So a naive board `TabGroup` tabs straight
through fusion's hidden views (`ActiveView::Mixer|Waves|PianoRoll`, switched by
`Display::None`) and any disabled control. The current-state cut is *medium* (below);
the general "hidden/disabled always excluded" promise is a genuine cross-crate subsystem.

**The mechanism, if/when reopened** (so the next session starts warm):
- **Hidden views → active per-view non-modal `TabGroup`** on the active `ViewRoot`
  only (views.rs:228), added/removed in the same central op that flips `Display`
  (views.rs:538). One source of truth; dynamically-spawned Waves controls participate
  automatically; no per-widget `TabIndex`↔view sync. Do NOT put a `TabGroup` on the
  outer board/content root (inactive views would be gathered through it). Reject
  option (a) descendant-`TabIndex`-sync (two sources of truth, misses new descendants)
  and option (b) forking bevy's navigation (`FeathersPlugins` installs
  `TabNavigationPlugin`; `gather_focusable` is private — an app-owned fork).
- **Disabled controls → remove `TabIndex` in CTK** (not `TabIndex(-1)`: bevy's
  click-to-focus `acquire_focus` treats any entity carrying `TabIndex` as focusable
  regardless of value). Precedent: ctk mixer.rs:3592. Dynamic disabling would need a
  CTK-owned "desired tab index" to restore from — but **fusion has no live
  `InteractionDisabled` transitions today** (only the already-`TabIndex`-stripped
  master spacers), so a generic disabled-sync framework is speculative now.
- **Modal capture:** `BoardInputSystems` does NOT gate bevy tab navigation (bevy
  handles Tab in focused-input dispatch, outside that Update set), so correctness must
  be structural — suspend/remove board non-modal groups while `ModalCapture` is active.
- **Stale-focus sanitation is mandatory:** on view switch, clear focus if it belongs to
  the outgoing view (else Space/arrows still reach the now-hidden focused widget, and
  the next Tab hits bevy's "current focus has no group" path); preserve it if it's
  persistent chrome; do NOT auto-focus the new view (agent/programmatic view changes
  must not steal focus). Same rule when a focused widget becomes disabled.
- **UX order:** footer group (lower order) → active-view group → wrap. Include the
  4-control transport footer; do NOT silently pull in the pointer-only menu (that needs
  its own Arrow/Escape keyboard-menu design).

**Reopen triggers:** a concrete keyboard-only/accessibility-dependent user; keyboard
accessibility as a release requirement; a pointerless target; ABP-exposed focus/control
topology (which WOULD make focusability agent-legible → then it earns its place); or a
real grouped/directional keyboard design rather than 162 sequential stops.

**Requester modal groups — PARTIALLY shipped 2026-07-21 (ctk 0.14.1).** The CTK file
requester captured the keyboard but its root (and its nested overwrite-prompt overlay) had
**no modal `TabGroup`** (unlike settings.rs). Added `TabGroup::modal()` to both
(file_requester.rs `spawn_requester` + `spawn_overwrite_prompt`; the prompt is a child of
the requester's modal group but its own nested modal group, which bevy's `gather_focusable`
correctly excludes from the parent). This makes requester-internal Tab work and is a
prerequisite the board work needs. **But a fresh-thread cold review found modal-group-alone
is NOT full containment:** a click on non-focusable dialog chrome (backdrop/title/blank)
has no `TabIndex` ancestor, so bevy's `click_to_focus` clears `InputFocus` to the window;
with no focus, `navigate` can't resolve the modal group and falls back to non-modal groups.
Harmless **today** (no non-modal board group exists → Tab does nothing), but once the board
group above exists, Tab would escape after such a click. **Full board-escape prevention
additionally requires focus-retention-on-backdrop** — the same "explicit stale-focus
clearing/eviction" this §16 mechanism already lists — so it is folded into the board-TabGroup
work: the modal groups are the shipped half, the focus-sanitation is the deferred half, and
both are required before any board non-modal group ships.

---

## 17. Atomic RT bank+stop+seek (finding #5) — SHIPPED via the ABP `app.song.load` verb (2026-07-22)

**STATUS: DELIVERED.** The zero-block guarantee shipped as part of the `app.song.load` verb
(fusion's first ABP surface), built 2026-07-22 across four converged phases — plan retired
from cos's tree. That plan predates both the `gui/` → `desktop/` move and the
fusion → studio rename, so the path no longer exists; read it with
`git -C $COSMIX show bb823bb^:gui/apps/fusion/_plan/2026-07-21-amp-song-load-verb.md`:

- **P1** app-port stand-up (ctk 0.15.0, fusion 0.2.0, `b052e2f`): ctk `AppPortPlugin` router
  (single-reply-owner, typed one-shot verb handlers) + `WidgetControlPlugin`; fusion becomes
  a discoverable ABP citizen via `app.describe` (bridge identity `midiseq-bevy-<pid>`).
- **P2** load-command convergence (fusion 0.2.1, `4b44d55`): one `LoadSongCommand` →
  transactional `load_one` (build→submit→commit; a pre-commit failure leaves the document
  untouched — fixed a latent half-apply in the old `open_song`).
- **P3** the RT barrier (musicd 0.21.0, fusion 0.2.2, `5957920`): `SongBankSwap { bank, load }`
  tagged ring + `MixerEngine::stop_at_zero()` + the `run_block` barrier (after the control
  drain, gated on swap acceptance) — a load renders its first block stopped at frame zero even
  over a stale queued Play; edits preserve transport. This IS the finding-#5 mechanism.
- **P4** expose the verb (ctk 0.15.1, fusion 0.2.3, `62450a5`): constrained-root path
  authorisation (canonicalize + component-wise ancestor check defeats `..`/absolute/in-root
  symlink escape; format allowlist; separate soundfont-root gate that never opens a denied
  font), the synchronous handler through the shared `load_one` (rc 0/10/11), `app.describe`
  advertises verbs. Owner-ratified trust model: constrained song roots, base-port-only.

Each phase independently cold-reviewed by a fresh adversarial subagent (codex was rate-limited
mid-build) and converged. **RT/verb follow-ups tracked here (none blocking):** (a) bring the
stem path's coalesce-to-newest to the song-swap drain — a PRE-EXISTING pure-edit-batch
playhead-rewind the P3 change neither introduces nor worsens; (b) one-block `applied_rev`
reporting skew after a load (self-corrects next block; cosmetic); (c) the verb's parse→build
runs inline on the Bevy thread — accepted (files live under owner roots; off-thread build is a
later optimisation).

--- historical record (the deferral that led here) ---

Handoff item #5. Design + earned consult run (codex, verified vs the RT source);
**owner decided** to defer the standalone RT patch and deliver the zero-block guarantee
**as part of a future ABP song-load verb**. No code shipped — record + plan below.

**The hazard (real but narrow).** The RT block order is drain-bank-swaps →
drain-transport-commands → render (mixer_host.rs:392/397/443/504). Loading a song does
`SongEditor::ship_bank` (lock-free bank ring) while the load's `AudioIntent::Reset` →
Stop+Seek(0) travel the transport-command path; the two aren't synchronized RT-side. If a
block boundary sees the new bank but not yet the Stop+Seek, it renders the new song at the
inherited playhead for ~one 128-frame block (2.67 ms @ 48 kHz; no hard upper bound during a
UI scheduling stall). **Correction that narrows it further:** a *stopped* load is harmless
too — while stopped the engine renders `source_idle`, not song audio (mixer.rs:1481), so the
cold bank is silent. The ONLY exposed path is **Open Song while transport is actively
playing**, and only if the new song has audible starting/chased notes. Not memory-unsafe —
bounded audible incorrectness. No observed report.

**Why not the standalone RT patch now.** It touches the highest-risk code in the tree
(lock-free RT audio) for an unreported, narrow glitch, and advances none of the three
agent-first criteria. Deferred.

**Why the ABP-song-load-verb framing (owner's call).** An agent/ABP `song.load` verb makes
while-playing loads *routine and deterministic*, so a zero-block ("install bank stopped at
zero") guarantee becomes part of that verb's **public contract** — and a structured,
agent-operable song-load path DOES advance legibility/modifiability. So #5 is not a bare RT
patch; it is one guarantee the song-load verb must provide. Building the verb is its own
design pass (verb shape, app-control integration, authorization) — not started here.

**The RT mechanism the verb must implement (captured so it's ready):**
- **Tag the song-swap payload load-vs-edit.** On a *load* swap, force stopped + hard-seek-0
  at the **pre-render barrier, AFTER ordinary command drain** (so a stale queued Play can't
  restart before render); newest bank wins, any load tag in a drained batch makes the batch
  reset. Do NOT reset on *edit* swaps — piano-roll edits, undo/redo, and soundfont replace
  all call `ship_bank` (editor.rs:250) and MUST preserve the playhead (load-bearing).
- **Keep the existing `AudioIntent::Reset` Stop+Seek writes** — the RT barrier stops audio
  leakage, but the revisioned writes still converge `transport.state`/`transport.position`/
  CTK desired-state/UI; removing them would desync store and engine.
- Reject option (a) "bundle bank into one `RtCommand`" — `RtCommand` is `Copy` with no owning
  allocations (mixer_host.rs:151), producers are split (editor owns bank ring, transport owns
  control ring), displaced banks return off-RT; bundling is a producer-ownership redesign.
- **Deterministic test** (no CPAL/timing flakiness) via `RtState::run_block`: play at non-zero
  → push load-tagged bank, no Stop/Seek → run one block → assert position 0, silent, length
  updated, displaced bank returned off-RT; stale Play in same block still resets; edit-tagged
  bank preserves playing+position; multi-swap batch with any load tag resets.
- Size: bounded-but-multi-file (mixer_host + mixer + transport + editor + file_io + tests); no
  stem-path change (fusion can't open a stem session dynamically today).

**Reopen trigger:** the ABP song-load verb work (or an observed load-while-playing glitch
sooner). Adjacent gaps to settle with it: ring-full/rebuild-failure, active-scrub-reject,
not-ready retained Reset (action.rs:217) — the same convergence gaps that make a full "atomic
song load" claim medium-sized.

---

## 18. Menus trip the Arc-3 gate — build `cosmix-actions` + the `.mix` keymap + menu↔ActionId unification (2026-07-22, SPEC — awaiting build)

**Status:** SPEC / PROPOSED. Not yet built, not yet Codex-reviewed (cold review
unavailable until 2026-07-25 — this is the pre-review record). Ratified direction
from Mark: treat **menu-completeness** as the trigger that reopens Arc 3 (§12) and
build the data keymap now.

**Question that prompted it:** "how do we deal with keyboard shortcuts — for every
menu option and most actions (Space pauses playback, etc.)?"

### 18.1 Why this reopens Arc 3 on a *different* rationale

Arc 3 (§12) deferred the `cosmix-actions` extraction behind the gate **"a second app
independently has semantic command ingress from ≥2 real sources with enough
duplicated implementation that moving it removes code."** Fable never adopted the
pattern, so that gate stayed unmet, and it still is — Fable is **not** a consumer
yet.

"Every menu option needs a shortcut" trips the gate a **different** way: not
*multiplicity across apps* but **breadth within one app**. Fusion alone, once its
menus are populated, has a combinatorial `menu-items × shortcuts × ABP-verbs` surface
that today is dispatched through **three unrelated id/dispatch worlds**:

1. `MenuItemDef.id: &'static str` → `MenuActivated { id }` — **no accelerator field**
   (`gui/ctk/src/menu.rs`); menus can't even render a shortcut hint.
2. Fusion's `Action` enum (4 variants, transport-only) → `ActionRequest`
   (`gui/apps/fusion/src/action.rs`); a **closed** enum that cannot name "every menu
   option."
3. A hand-rolled `match key.key_code == KeyCode::Space` with bespoke focus/modal
   suppression (`action.rs:~172`) — the *only* keyboard→action path, one key wide.

Adding shortcuts for N menu items on top of this means growing all three by hand, in
lockstep, per item. **That** is the duplicated implementation the extraction removes —
so menu-completeness satisfies the *spirit* of the Arc-3 gate (extraction removes real
duplicated code) even though the literal "second app" wording is unmet. Mark ratifies
the reframe: **extract on Fusion's menu breadth; Fable adopts `cosmix-actions` later**,
when it grows its own command ingress. We are honest that this reverses §12's
"not until two apps" — on a breadth-not-multiplicity basis, deliberately.

### 18.2 The central refactor — `Action` closed enum → open `ActionId` registry

"Every menu option" is an **open** vocabulary; a 4-variant enum can't hold it. So the
spine changes shape:

- **`ActionId`** = an interned / `&'static str` id (exactly the shape `MenuItemDef.id`
  *already* is). **The menu id becomes the ActionId** — the two id spaces collapse
  into one. No parallel enum-to-string bridge.
- **`ActionDef { id, label, args_schema, enabled_predicate }`** — the per-app registry
  entry (§4). Typed args validated at ingress (§4's closed-vocabulary discipline is
  kept; open id space ≠ unvalidated payloads).
- Fusion's transport variants become **registered `ActionDef`s**, not enum arms.
  `apply_audio_intents` still consumes typed `AudioIntent`; only the *front* (naming +
  resolution) generalises.

### 18.3 Build list (per §3.5 and §4, now un-deferred)

1. **`cosmix-actions` — new core-first crate** (mesh-free, no Bevy): `ActionId`,
   `ActionDef`, `Binding`, `Keymap`, `resolve(input: RawInput, focus, keymap) →
   Resolved` (chords included), and **`.mix` (de)serialisation** of the keymap. This
   is exactly §4's crate, built now.
2. **The `.mix` keymap as data** (§3.5): layered **defaults ← per-app user `custom`**,
   hot-reloaded, one table describing every trigger surface. Ships with Fusion's
   default bindings (Space→transport-toggle, etc.). Same `ActionId`s are reachable
   from MIDI/OSC/ABP later — the keymap is the keyboard layer over one id space.
3. **One resolution system** replaces the hand-rolled Space match: `resolve()` takes
   `focus` (so a focused text field still eats Space — the existing focus-aware
   suppression moves *into* `resolve`) and runs under the same `UiMode`/`ModalCapture`
   gating (`run_if(in_state(...))`); a modal's own bindings (Esc/Enter) resolve within
   the modal's capture scope. Emits `ActionRequest`.
4. **`Source::Menu`** — extend the `Source` enum (currently Key/Mouse/Amp/Midi/Osc, 4
   of 5 unused). `MenuActivated { id }` maps to an `ActionRequest { action: id, source:
   Menu }` — menus become a **first-class producer** into the same broker, not a side
   channel.
5. **`MenuItemDef` carries the action + renders the accelerator by reverse-lookup.**
   Add an `action: ActionId` reference (or promote `id` to be the ActionId). The menu
   renders its shortcut hint by **reverse-looking-up the chord bound to that ActionId
   in the keymap** — it stores no accelerator of its own. Consequence: a user rebind in
   the `.mix` keymap updates the menu's displayed accelerator **for free**, and there is
   exactly one place a binding lives.

### 18.4 What stays deferred (do not pull in)

- **ABP `action.invoke id=… args=…` verb + `actions.*` props namespace** (§4, Phase
  2). Menus and keyboard don't need the mesh surface; the unification makes it a
  natural *later* step but it is **not** in this build. Keep the trust-boundary /
  session-naming BLOCKERs (cross-ref the interaction-service-broker ADR) out of the
  local keymap work.
- **Fable adoption** — Fable stays imperative (`ButtonInput → mutate BrowserState`)
  until it grows real command ingress; it consumes `cosmix-actions` then, not now.
- **MIDI/OSC binding tables** — the id space is designed to admit them (§3.5), but only
  the keyboard + menu + ABP-shaped `ActionRequest` layer is built here.

### 18.5 Acceptance criteria

- Fusion boots with **zero** hand-rolled `match KeyCode::*` in `action.rs`; Space,
  every transport action, and every menu item resolve through `cosmix-actions`.
- A menu item and its keyboard shortcut share **one** `ActionId`; deleting the binding
  from the `.mix` keymap removes the menu's accelerator hint with no code change.
- Editing `~/.config/cosmix/…` keymap `custom` and re-focusing hot-reloads the binding
  (no relaunch) — parity with the theme A3 reload story.
- A focused text field still swallows Space (focus suppression preserved through
  `resolve`); a modal's Esc/Enter resolve only within the modal.
- `cosmix-actions` has **no** Bevy or mesh dependency (core-first, per §4); clippy
  `--workspace --all-targets` clean.

### 18.6 Coordination & risk

- **`menu.rs` is under active edit by the concurrent theme session** (A3 in flight,
  uncommitted in the main `$COSMIX` tree). Adding `action: ActionId` to `MenuItemDef`
  touches the **same struct** that session is theming. **Land after that settles or
  coordinate the struct change** — do not race it. This is the one real collision in
  the build list; the rest (`cosmix-actions` crate, `action.rs`, keymap asset) is
  Fusion-local and clear.
  - *Lapsed (2026-07-23):* the concurrent work landed as ctk 0.16.2 ("menu chrome
    themed" — that was the chrome item, not A3; A3 live-reload is still unbuilt)
    and `menu.rs` is clean in git. The struct change no longer races anyone.
- **Un-deferral (2026-07-23, Mark-directed):** §18.4's two deferrals are lifted —
  the build now includes the ABP action surface for every menu option and Fable's
  `cosmix-actions` adoption (fable gains a menu bar + its first app port, so the
  literal "second app with real command ingress" gate is met). Shipped
  2026-07-23; current implementation lives in `cosmix-actions`,
  `desktop/ctk/src/{menu,action_control}.rs`, and the Studio/FileMgr action
  modules.

---

## Sources

Research briefs (this session, 2026-07-20): Bevy 0.19 event model
(bevy.org/news/bevy-0-19, `bevy::input_focus`, leafwing-input-manager 0.21,
bevy_enhanced_input 0.26); KDE Plasma/KWin (Gräßlin "How input works",
KGlobalAccel D-Bus interface, KActionCollection); System76 COSMIC
(cosmic-comp `filter_keyboard_input`, smithay `FilterResult`, cosmic-config RON
shortcuts, cosmic-protocols Toplevel-Management). Full source lists in the
session transcript.
