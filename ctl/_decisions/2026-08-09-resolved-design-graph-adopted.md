# ADR: CosMix Resolved Design Graph adopted as the desktop design system

- **Date:** 2026-08-09
- **Status:** **ACCEPTED** (Mark, 2026-08-09 — "Accept all ten
  recommendations as stated, commission the SPEC").
- **Decision authority:** Mark (styling architecture is a
  user-facing-taste + toolkit-boundary + public-API call).
- **Basis:** `_doc/2026-08-09-cosmix-desktop-design-system.md` (rev 5,
  converged through six cold codex review rounds on thread
  019fe401-9008-7df3-81c1-4f72a5393c6d). That document carries the full
  evidence and architecture; this ADR records only the decisions.
- **Neighbours:** succeeds the rejected flair overlay
  (`2026-08-09-bevy-flair-css-overlay.md`); subordinate to
  `2026-07-22-cosmix-visual-identity-own-palette.md` (unamended — see
  decision 5) and the Bevy+CTK toolkit decision (2026-07-30).

## The ten decisions (all per the proposal's §7 recommendations)

1. **Direction adopted.** The Resolved Design Graph is the CosMix styling
   architecture: `.mix` DTCG-shaped source (primitives, atomic
   surface+foreground pairs, cva-shaped component mappings, resolver-shaped
   modifiers) → dependency-light headless compiler → immutable revisioned
   `ResolvedDesign` → single-writer family resolvers. No selectors, no
   cascade, no matching engine, no external styling dependency. The SPEC
   is commissioned as **Chapter 19**.
2. **Single-writer enforcement = audited policy**: ownership registry +
   per-writer mutation counters/schedule sentinels + removal necessity
   test, accepted as sufficient knowing Bevy cannot make it structural.
3. **v1 context scope = global + per-instance only.** A
   `SurfaceLevel`-style ambient axis is deferred; if added later it is one
   enum component + one table axis, never a cascade.
4. **Semantic vocabulary migrates** to a shadcn-style general ~20-name
   closed tier (with pairs), replacing the audio-console `ctk.*` names as
   the public token API, alias window until migration slice 3; console
   names survive only as component-mapping-internal names where genuinely
   domain-specific.
5. **Web emitter = primitive tier + colour method + scheme structure
   only.** The 2026-07-22 ADR's "not the vocabularies" clause stands
   unamended; semantic-tier emission would need that ADR amended first.
   The emitter consumes only the named shared context — per-app overlays
   structurally excluded.
6. **Raw `*_sized(px, px)` instance geometry stays** as a declared
   exception: allowed only via explicit sized-constructor APIs, stamped
   `entity-local` in provenance. Migration to scale steps is mechanical
   later work, not v1.
7. **v0 compatibility window accepted** (no fleet flag-day): v0 subset
   stays authored until v0 readers are gone; readers before writers; the
   compiler's v0↔v1 equivalence gate (normative relation, fatal on drift)
   holds the two representations to one value.
8. **Scope is desktop-wide with central registration:** the ownership
   registry and mapping tables cover CTK and app-defined families
   (filemgr, studio, future apps) under one SPEC; family schemas live in
   the shared headless compiler crate, which is thereby a closed central
   family-schema registry. An app-owned type-keyed extension mechanism is
   named deferrable follow-up.
9. **First implementation slice = button-first** (rides ctk 0.48.0's
   nearest-single-writer button work; metrics enter the live lane,
   superseding the F-metrics follow-up).
10. **SPEC placement = numbered chapter now**: SPEC 19,
    `_spec/2026-08-09-19-cosmix-design-system.md`.

## Consequences

- Chapter 19 is the normative contract; the proposal doc becomes its
  design rationale and stays put in `_doc/`.
- Implementation is **not** authorised by this ADR beyond what each
  migration slice's own session decides; slices land one at a time,
  slice 1 = compiler crate + button family + writer-attribution audit.
- Button Phases 2–4 (`_plan/2026-08-08-ctk-button-component.md`) proceed
  independently; the ButtonDef **API shape** (variant/size axes,
  builder) is durable under this architecture, but the public `ctk.*`
  token names those phases touch are subject to decision 4's vocabulary
  migration (alias window closing at slice 3) — legacy names to
  migrate, not a durable surface.
- The flair evaluation's writer inventory, contention table, and audit
  machinery design are carried forward as live input to slice 1.
