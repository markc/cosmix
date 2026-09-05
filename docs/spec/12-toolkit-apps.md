---
title: Toolkit, Applications and Design Contracts
chapter: 12
version: 0.1.0
status: draft
date: 2026-09-05
---

# Toolkit, applications and design contracts

Native applications render their own UI using CTK and Bevy. Bus exposes semantic operations and observable application state. The remote `ui.*` widget renderer, its fenced-widget grammar and conformance tiers are historical and must not be treated as the current application interface.

Baseline: public revision `96d12fdf`. This chapter is a contract summary and implementation profile; it does not certify every widget against the complete design-system proposal. The [retained resolved-design format](12a-design-format.md) preserves the exact source grammar, crosswalk, validation, derivation, ownership and gate requirements behind this summary.

## Application ownership

**UI-001 — Semantic control.** Application state and operations belong to the application. Prefer an explicit operation, such as opening a document, over synthesising input when that operation is exposed. The compositor owns window protocol state; the application owns its document and unsaved-work semantics. A Bus disconnect must not move rendering or document authority into the broker.

**UI-002 — Behaviour and presentation.** Rust owns widget structure, allowed axes, parts and behaviour. Design data selects values within that schema. Accessibility, keyboard focus, modal capture and drag/drop are behavioural contracts, not consequences of successful theme compilation.

Evidence: [CTK application control](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/desktop/ctk/src/app_control.rs), [Bus integration](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/desktop/ctk/src/bus.rs), [modal capture](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/desktop/ctk/src/modal_capture.rs), and [app shell](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/desktop/ctk/src/dcs_app_shell.rs). Existing modules provide mechanisms; application-level conformance still needs scenarios.

## Source and compilation

**UI-003 — Versioned strict data.** Design source is versioned Mix strict data. Preserve the authored v0 subset for the declared compatibility window. A v1 compiler checks the explicit v0 crosswalk rather than approximating old colours or dimensions. Unknown versions and invalid values must yield diagnostics; they must not silently produce a claimed v1 design.

**UI-004 — Typed graph.** References flow from family mapping to semantic tokens to primitives. Cycles and ambiguous equal-specificity writes are rejected. Text surfaces are surface/foreground pairs. Metrics carry explicit `px`, `step` or `ratio` kinds; ratios cannot occupy length positions. A scale step resolves to a length. Rust-defined enums and part schemas bound the reachable mapping space.

The radius scale remains derived from the authored base as `max(base−4,0)`, `max(base−2,0)`, `base`, `base+4`; a second authored radius scale would create competing truth. The legacy `corner_radius` field is its own compatibility metric, not the nearest radius step. Numeric validity is assessed after the strict-data number has parsed to a double; step indices at or above 2^53 are rejected because integer identity is not recoverable.

**UI-005 — Complete cells.** Every resolver-owned property requires an authored base. More specific mappings override it; explicit null restores it. Equal-specificity overlap is an error. New enum values must be covered explicitly or identified as inheriting base, with the declared warning/fatal coverage policy. Spawn-time placeholder values are not a fallback styling authority.

The headless [design compiler](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/desktop/crates/cosmix-design/src/compiler.rs), [source model](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/desktop/crates/cosmix-design/src/source.rs) and [mapping model](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/desktop/crates/cosmix-design/src/mapping_model.rs) exist. The public resolved table currently includes the button family; this does not establish migration of every CTK or application family.

## Colour, overrides and derivations

**UI-006 — Delivered contrast.** V1 colours use explicit OKLCH authoring. Contrast is evaluated after gamut mapping to sRGB and the declared compositing context. Text pairs and non-text focus indicators have distinct postconditions. A successful compile must cover all permitted substitutions, not just the default theme.

**UI-007 — Closed overrides.** Runtime instance overrides are typed and family-specific: instance override, otherwise resolved table. No implicit ancestor/style cascade is introduced. A pair override moves its foreground with its surface. The artifact classifies each candidate substitution as admitted or excluded, retaining the exclusion reason. Runtime evaluation refuses an excluded candidate instead of silently substituting an unrelated value.

**UI-008 — Deterministic derivations.** Data may call registered typed derivations, not supply arbitrary expressions. A retained recipe has zero or one explicit override-substitutable input. Validate preconditions and output postconditions during compilation and any permitted runtime re-evaluation. Runtime recipes consume the compiled artifact, not raw source or a separately reconstructed admission rule.

For the retained focus-ring algorithm, the seed and output are opaque; search lightness in 1/1000 increments plus each direction's endpoint, holding nominal hue/chroma before gamut mapping. Choose the first passing 3:1 candidate by integer step count, preferring lighter on a tie. Record both step index and actual lightness travel. More than 300 steps warns; it is not a compile failure. These are specific algorithm contracts, not a universal claim that all themes meet accessibility requirements.

Evidence: [colour model](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/desktop/crates/cosmix-design/src/colour_model.rs), [recipes](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/desktop/crates/cosmix-design/src/recipe_compiler.rs), and [resolved artifact](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/desktop/crates/cosmix-design/src/design_model.rs). Exact palettes, crosswalk field sets and derivation registry entries require compatibility fixtures when changed; a prose simplification does not authorise deleting them.

## State, invalidation and writer ownership

**UI-009 — Normalised state.** Interaction precedence is disabled, pressed, hovered, resting; checked, selected and visible focus are orthogonal. State normalisation must observe removed markers and both the old and new focus holders. It does not paint.

**UI-010 — Complete invalidation.** Re-resolve on relevant state, variant/size, added/changed/removed override, late part insertion, reparenting and design revision changes. Typography uses family/part/variant/size rather than interaction state; interaction must not unexpectedly reflow text by changing font size.

**UI-011 — Single writer.** Each managed family/part/component has a declared presentation owner. Procedural geometry remains on separate widget-owned entities. Enforce this by writer-attribution tests and review; Bevy component access does not make the policy a Rust type-system guarantee. Settled widgets avoid spurious writes and text rerender. Change filtering may still scan entities; do not claim zero work solely because values remain equal.

Evidence: [button implementation](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/desktop/ctk/src/button.rs), [CTK design adapter](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/desktop/ctk/src/design.rs) and [button gate](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/desktop/ctk/tests/design_button_gate.rs). Ownership for all other families and application painters remains to be attested.

## Apply, wake and diagnostics

**UI-012 — Atomic last-good apply.** Compile a replacement completely before publishing its revision. Failed parse/compile retains the last good design and publishes an error distinct from the live revision. Avoid retrying an unchanged failure every frame; diagnostics must not dump source containing application-private content.

The CTK adapter stores source identity/generation/fingerprint, attempted/applied keys, compile status and the last-good artifact. It compiles synchronously during `Update` when the key changes. It currently builds context with `Contrast::Normal` and `app: None`; compiler support for axes is not equivalent to end-user high-contrast or per-app overlay integration.

**UI-013 — Reactive apps.** Background changes need an event/wake path so an idle UI refreshes without continuous polling. Theme source updates and Bus notifications are inputs, not permission to put networking in the frame-critical path. Display diagnostics should distinguish source rejection, compile failure, unsupported context and failed delivery.

Adapters may translate compiled values into backend representations, but must not become another styling authority. Font assets cross backend boundaries as assets/handles. The broader adapter set, public design introspection endpoints and migration of all families remain intended work until each is evidenced.

## Verification and evolution

| Contract group | Required evidence |
|---|---|
| UI-001–002 | Semantic operation scenarios; keyboard, accessibility, modal and drag/drop tests per application |
| UI-003–005 | Version/crosswalk compatibility fixtures; invalid references, units, coverage and ambiguous-rule rejection |
| UI-006–008 | Gamut/compositing tests; complete admitted override product; derivation pre/postcondition failures and ring threshold boundaries |
| UI-009–011 | Each invalidation trigger, old/new focus, override removal, late/reparented parts and writer attribution |
| UI-012–013 | Bad-replacement retains revision, valid replacement wakes idle app, unchanged input avoids recompile, settled widgets avoid writes |

Tests were not run for this audit. The old design-system chapter's full acceptance catalogue remains a traceability input; any requirement not yet mapped to an implementation/test is an open conformance item, not implicitly waived. Animation tokens, ambient surface axes, open app-owned schema extension and full alternative backend coverage remain deferred. Pre-GA changes must name the format/API affected and update the relevant fixtures in the same change.
