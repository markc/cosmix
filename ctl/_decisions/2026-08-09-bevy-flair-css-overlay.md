# ADR: bevy_flair as an opt-in CSS styling overlay for CTK — REJECTED

- **Date:** 2026-08-09
- **Status:** **REJECTED at the pre-Phase-0 gate** (Mark, 2026-08-09). The
  decision bar was "at least 9/10 positive"; the evaluation's own best case
  was a conditional go requiring two-sided ownership fencing, hostile
  contention tests, and a removal rehearsal — complexity as a cost of
  admission, not a clean win. No spike code was ever written, so the no-go
  kill switch is trivially discharged (nothing to remove). The evaluation
  stands as the record of *why*: CSS's cascade/selector/inheritance model is
  the part that contends with ECS ownership; the parts we wanted (named
  tokens, variants, hot-swap) don't require CSS. **Successor work:** a
  spec-driven CosMix desktop design system, commissioned by Mark 2026-08-09
  in the same breath as this rejection — learn flair's lesson, don't mimic
  CSS. The body below is preserved as written at proposal time.
- **Decision authority:** Mark (styling architecture is a user-facing-taste +
  toolkit-boundary call).
- **Trigger:** Mark's evaluation prompt
  (`_plan/2026-08-08-bevy_flair-evaluation-prompt.md`), raised mid-way through
  the CTK canonical-button arc — decide whether widget styling internals
  should move to CSS before the button Phase 2/3 migrations broaden the
  hand-rolled surface. Investigation executed 2026-08-09 (source read of
  bevy_flair 0.8.0 + ctk, scratch compile probe); full findings + phases +
  evidence: `_plan/2026-08-09-bevy-flair-adoption-evaluation.md` (rev 3,
  revised through three cold-review rounds).
- **Relationship to neighbours:** subordinate to
  `2026-07-22-cosmix-visual-identity-own-palette.md` (the palette is cosmix's
  own, delivered as `.mix` strict-data — this ADR must not move the source of
  truth) and to the toolkit decision (Bevy + CTK confirmed 2026-07-30, no
  re-evaluation until JMAP mail ships — flair is an overlay *on* CTK, not a
  toolkit change). Consistent with
  `2026-06-18-canonical-css-base-site-methodology.md` on the web side, which
  the shared `--ctk-*` variable emission would feed.

---

## Decision (proposed)

Adopt **bevy_flair 0.8.0** as a **default-off, ownership-fenced CSS styling
overlay** for CTK chrome — *not* as a transparent styling layer over existing
widgets — subject to the two gates above.

1. **The `.mix` theme file stays the single source of truth.** CSS contains
   no authoritative colour or metric literals — its property declarations
   consume `var(--ctk-*)` variables regenerated from the resolved
   `ThemeSpec` on every theme apply (mutable root `InlineStyle`, keyed on
   `ThemeState.revision`; enforced by a literal-free-CSS test gate). Schemes
   × modes are root classes; `prefers-color-scheme` is not used (flair
   sources it from `Window.window_theme` — a second mode authority next to
   `ThemeState`; explicit classes avoid maintaining two). Resolve-time WCAG
   derivations (the selected-text knockout pair) emit as finished variables;
   **repaint-time `contrast_safe_lift`-derived state colours stay
   Rust-owned** in the pilot — flair has no colour arithmetic; simple
   token-valued state styling (ToggleButton's checked background is a plain
   palette swap) is flair-owned, which is precisely the pilot's `:checked`
   evidence. Intentional-alpha tokens (scrim, alpha 0.62/0.36 by design) stay
   token-painted; the flair #45 low-alpha workaround is scoped to the
   properties the Phase 0 reproduction identifies, never a universal
   opacity ban.
2. **Two-sided ownership fencing, per property family.** flair auto-inserts
   `Styled` on every `Node`/`TextSpan` and inherits stylesheets to
   descendants, so a CTK-side marker alone cannot fence it. The rule is
   enforced from both sides: *flair side* — `Styled::Block` at every
   CTK-owned root nested under a styled subtree, plus a parser-based
   stylesheet lint asserting metrics-scoped sheets declare no colour/font
   properties; *CTK side* — ownership markers are **per property family**
   (`FlairOwnsNode` / `FlairOwnsPaint` / `FlairOwnsTypography`, or one
   bitflags component), each checked by exactly the CTK systems writing
   that family — an entity-wide marker cannot express "flair owns Node,
   Rust owns BackgroundColor". Every entity whose component flair styles
   carries the matching marker, including sheet-reachable descendants (a
   root marker does not excuse a child label). Hostile broad-selector +
   marker-placement tests and a component-level contention table (module ×
   system × component × trigger × owner) ship with the pilot, covering both
   the settled-style regime (change-driven, no re-enforcement) and the
   animation regime (flair writes every frame while a transition runs).
3. **The no-CSS boundary is property-level, not module-level.**
   Value-driven geometry and pixel painting stay procedural permanently
   (meter level lanes, knob rotation, wave/piano-roll/topology model
   geometry, indeterminate progress, SvgColor tinting — mostly
   change-driven, not per-frame); static backgrounds, borders, dimensions
   and state classes on the same widgets are separately assessable per the
   plan's Phase 3 matrix.
4. **Bevy upgrades are never blocked on flair.** If flair does not compile
   against a new Bevy pin within our deliberate compatibility-pass window,
   the `css` feature is disabled for that release. (Standing rule; flair
   currently has no 0.20 tracking found.)
5. **Deletable by construction — enforced, not assumed.** The overlay sits
   behind the default-off `css` feature; **every migration retains and
   tests the css-off Rust styling path** (pixel parity in both feature
   states); no fallback geometry/paint is deleted while the overlay is
   optional; the Phase 2 gate includes a **removal rehearsal** (delete
   flair, run the full parity suite) before this ADR can be accepted. On
   go, the pinned tag is vendored per our pinned-ecosystem-crate practice.

## Why (the three criteria)

- **Legible:** a hand-written stylesheet referencing named `--ctk-*` variables
  is more inspectable than scattered spawn-literal geometry — *provided* the
  variables trace to the `.mix` file an agent can already read and mutate.
  That proviso is why the bridge direction (`.mix` → CSS, never the reverse)
  is a decision clause, not an implementation detail.
- **Modifiable:** the genuinely new capability is **hot-swappable geometry**.
  Today only button metrics reconcile live; the legacy `CtkThemeMetrics`
  fields are inert (parsed, consumed by nothing — they don't take effect
  even after a relaunch), and the rest of the geometry is hardcoded spawn
  literals or explicit size arguments. The honest comparison is flair-CSS
  vs. simply wiring the existing metrics lane up (follow-up F-metrics in
  the plan) — the Phase 2 pilot must beat that bar, not a strawman.
- **Reconstructible:** neutral-to-negative — one more third-party crate with a
  poor bus factor (112/121 commits one maintainer, main unmoved since
  2026-06-22) in the reconstruction path, mitigated by default-off +
  vendoring + the enforced deletability clause. This is why the ADR is an
  *experiment* framing with the adoption decision deferred to the Phase 2
  gate.

## What was rejected

- **Transparent overlay** (flair restyling existing widgets in place):
  rejected on evidence — ownership contention with the CTK writer inventory
  (`update_button_style`/`update_button_metrics`/
  `reconcile_button_label_fonts`/`apply_ctk_typography`/
  `update_interaction_styles`/`update_toggle_style` and the
  meter/fader/knob geometry writers), with no conflict policy on either
  side and dual interaction-state inputs (legacy `Interaction` + modern
  components feeding one internal `StyleData`).
- **CSS as the palette source** (or generated `.css` theme assets): rejected —
  violates the 2026-07-22 ADR's source-of-truth clause and the `.mix`
  mandate; also technically unnecessary since `InlineStyle` variables
  hot-propagate without asset reloads (disk `.css` reload additionally
  needs Bevy's `file_watcher`, enabled only for the Phase 0 example).
- **Reproducing the AA machinery in CSS:** impossible in flair (no
  `color-mix()`, no relative colours — open issue #33) and undesirable — the
  contrast logic is tested Rust; `contrast_safe_lift`-derived repaint-time
  state colours stay Rust-owned (clause 1's split: token-valued state
  swaps like the checked background are flair-owned).
- **CTK-side-only ownership marker:** rejected during review — flair's
  auto-`Styled` + sheet inheritance means only `Styled::Block` fencing on
  flair's own terms actually bounds a subtree (decision clause 2).

## Phase 0 results (fill-in — authorises Phases 1–2 only)

> From the runtime spike (plan rev 3 Phase 0; throwaway branch, existing
> widget_gallery + an added Feathers button restyled with both writers
> live + isolated control panel, disk reload via the non-default
> `css-dev = ["css", "bevy/file_watcher"]` feature).

- Build inside real ctk graph, both feature sets: ☐ pass / ☐ fail —
- Ownership characterisation (settled / animated / deliberate-retake): —
- Disk `.css` hot-reload; `InlineStyle` var propagation without reload: —
- Pseudo-classes on modern components; congruent AND contradictory legacy
  `Interaction` states coherent: —
- AccessKit snapshot diff: —
- flair #45 reproduction — affected properties: —
- Binary size delta / frame-time delta: —
- **Amendment A** (contention closed-form completed at Phase 2, not
  pre-plan): ☐ accepted / ☐ rejected —
- **Phase 2 pilot choice:** ☐ toggle_button (default) / ☐ button-metrics —
  a redirect to buttons requires revising the plan's Phase 2 and this ADR's
  Phase 2 fill-in *before* proceeding (both are written toggle-first)
- **Gate outcome:** ☐ proceed to Phases 1–2 / ☐ no-go (record + close)

## Phase 2 results (fill-in — the adoption decision)

> From the toggle_button pilot + fencing (plan rev 3 Phase 2).

- Pixel parity, css on AND off, per sized variant: —
- `.mix` metric hot-swap without relaunch (ThemeFile extension landed): —
- Hostile-selector + both-regime contention tests: —
- Removal rehearsal (flair deleted, full parity suite green): —
- **Gate outcome:** ☐ ADOPT (mark ADR accepted, Phase 3 rollout opens) /
  ☐ no-go (remove dependency + feature + spike code; record + close)

## Consequences (if adopted at the Phase 2 gate)

- New default-off ctk feature `css`; pinned `bevy_flair = "=0.8.0"`; three
  small duplicate dep families accepted (itertools, rustc-hash,
  variadics_please); one shared `cssparser 0.37` with `body-view`.
- Property-family ownership markers + `Styled::Block` placement become part
  of every CTK styling system's and spawn helper's contract
  (writer-contention regressions keep them honest); the parser-based
  stylesheet lint joins the ctk test gates.
- Token→variable serialisation lives in a dependency-light shared layer
  (no Bevy, no ctk) with two adapters: native (ctk root `InlineStyle`) and
  web (invoked by the web deploy path only, consuming the shared cascade —
  never a desktop app's per-app overlay — written atomically). The drift
  test compares both adapters against the same pure token map. `cosmix-webd`
  gains no Bevy dependency; desktop apps never write web assets. The
  shared-values story from the 2026-07-22 ADR, useful independent of native
  adoption depth.
- Pilot scope: **toggle_button, whole-family** (within the prompt's
  fader-or-toggle choice; `:checked` exercises state styling hardest), with
  layered metric precedence — `.mix` supplies defaults, explicit
  `toggle_button_sized` per-instance dimensions remain entity-local
  overrides, parity asserted per variant. A button-metrics pilot is offered
  to Mark as an explicit prompt amendment — buttons are where
  live-reconciled metrics already exist — but is not substituted silently.
  Fader was passed over with reason (its `ThemeFile` schema already exists;
  it lacks consumers, and carries heavier procedural entanglement for equal
  evidentiary value), not because its inert metrics "prove nothing".
- On **no-go at any pre-adoption gate** (Phase 0, the Phase 1 bridge gate,
  or Phase 2): the dependency, feature, and spike code are removed from the
  tree — no dormant residue unless Mark separately approves a follow-up.
  (Post-adoption, Phase 3 has its own per-app rollback + global-freeze
  regime.)

## What this is NOT

Not a toolkit re-evaluation (Bevy + CTK stands), not a theming-source change
(`.mix` stands), not a migration commitment — an experiment whose adoption
decision sits at the Phase 2 gate, with kill switches at every gate and
full removal as the default no-go outcome.
