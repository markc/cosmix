---
title: Resolved Design Format — Retained Contract
chapter: 12a
version: 0.1.0
status: draft
date: 2026-09-05
---

# Resolved Design Format — Retained Contract

**UI-FORMAT-001 — Retained design-format contract.** The following sections preserve the exact v1 source, crosswalk, colour, mapping, typography, state, ownership, override, derivation, apply and conformance rules from the design-system contract. Section numbers are local and remain stable for existing clause references. This appendix supplies the detailed requirements behind the [toolkit chapter](12-toolkit-apps.md); its rules are not waived by that chapter's shorter summary.

Baseline source inspected: `96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be`. Retained intent and implementation evidence are separate: claims below about required gates, migration slices, entire-desktop scope and older shipped versions do not establish that all are implemented or passed at this baseline. In particular the CTK adapter pins normal contrast/no app overlay, and complete non-button family migration has not been attested. No test run is claimed by this publication.

The historical chapter number is 19; it is not remapped to numeric distribution ID 12. Original decision numbers identify prior accepted design choices, not missing runtime fields. Internal project decision links were omitted from the introduction; the format requirements themselves are preserved. Reassessing a requirement requires an explicit amendment, not an inference from current code.

## §1 Source format and versioning

1.1 The design source is `.mix` strict-data. It is DTCG-*shaped* —
groups, aliases, explicit types, composite records — but the encoding is
ours (Adobe precedent: borrow the shape, own the encoding). No JSON, no
CSS, no new DSL; the existing strict-data loader parses it.

1.2 The source MUST carry an explicit integer schema version field.
Today's `ThemeFile` has none and ignores unknown fields; that behaviour
is grandfathered as **v0**. All content defined by this chapter is **v1**
and MUST live under versioned sections that a v0 reader never parses.

1.3 **Mixed-version policy (honest by construction).** A genuinely old
v0 reader cannot report a version gap — it predates the version field.
The policy therefore leans on structure, not detection:

- the v0 subset stays present and authored for the whole compatibility
  window (decision 7 — no flag-day);
- readers roll out before writers;
- only upgraded readers report the version gap (via §11's
  `DesignCompileStatus`).

1.4 **v0↔v1 equivalence gate.** The compiler MUST verify, fatally on
drift, that the v0 subset and the v1 source resolve to the same values.
The equivalence relation is normative and fully computable:

- **Compared field set (frozen here):** every field the shipped v0
  reader consumes — the colour fields `surface`, `panel`,
  `master_panel`, `track`, `control`, `control_active`, `thumb`,
  `meter_green`, `meter_amber`, `meter_red`, `text`, `text_dim`,
  `border`, `row_hover`, `row_selected`, `row_selected_text`,
  `row_selected_text_dim`, `scrim`, `danger_surface`; the metric fields
  `control_gap`, `corner_radius`, `fader_width`, `fader_height`,
  `knob_size`, `meter_width`; and the selection/typography fields
  `scheme`, `mode`, `typography.family`, `typography.body_px`. Fields a
  v0 reader ignores are outside the relation.
- **Crosswalk (source-authored, compiler-enforced):** the v1 source
  carries a `v0_crosswalk` section mapping **every** field above to one
  v1 expression — a token reference, one half of a pair, a component
  mapping cell, a metric reference, or (for `scheme` and `mode`) a
  **modifier-axis selection** naming the §11.2 axis whose selected
  value the v0 string must match (e.g. whether v0 `surface` equals v1
  `base` or `card` is decided *in that section*, not by implementation
  judgement). A compared field with no crosswalk row, or a crosswalk
  row naming a field outside the set, is **fatal**. The crosswalk is
  comparison data only — it is not a token tier and nothing may
  reference it.
- **`corner_radius` is not a radius step.** v0's `corner_radius` is
  5 px and §2.2's derived scale at the shipped base is `2/4/6/10`, so
  no step equals it. That is not a defect in either: v0 carries
  `corner_radius` alongside its radius scale as a separate knob, and
  the compatibility window demands the old value exactly. Its crosswalk
  row therefore names a v1 metric of its own for the duration of the
  window; a crosswalk that reaches for the nearest step would move a
  shipped dimension in the name of tidiness.
- **Absence rule:** the v0 subset MUST be fully authored for the whole
  window — a compared field absent from the v0 block is **fatal** (an
  absent field would fall back to a v0 reader's compiled-in default,
  which the gate cannot see).
- **Compared context:** v0 predates the contrast axis and per-app
  overlays, so the gate compares the v0 block against the v1 resolution
  under the crosswalked `scheme` + `mode` with `contrast: normal` and
  **no** per-app overlay — the only context v0 can represent. Other
  contexts are outside the relation.
- **Conversion pipeline:** v1 OKLCH → gamut-map → sRGB → quantise to
  the 8-bit representation v0 stores (§3.2's order).
- **Tolerance:** colours byte-exact after quantisation; metrics —
  `typography.body_px` included — exact in px; `typography.family`
  byte-equal as a string; `scheme` and `mode` byte-equal between the v0
  string and the identifier of the selected v1 modifier-axis value
  (§11.2's axes).

"Equivalent" is thereby a computable predicate, not judgement:
independent implementations of this relation agree by construction.
Conformance is one implementation plus a fixture suite covering every
crosswalk row, both sides of at least one quantisation boundary, and —
proving the compared-context pin — at least one fixture in which high
contrast and at least one in which a per-app overlay *would* alter a
compared field, passing only because the gate compares the normal,
unoverlaid context.

1.5 The v0 block is legacy-format and exempt from v1 authoring rules
(including §3.1's hex prohibition). The exemption dies with the window.

1.6 **Modifier blocks — the authoring form of §11.2's axes.** The base
source declares unmodified values; each context is authored as a
**modifier block** whose `when` names one or more axis values it applies
under, carrying only the token paths it changes. Blocks are sparse by
construction: a modifier that restates a base value unchanged is a lint
warning — §2.5's no-ceremonial-indirection rule applied to contexts.

- The axes and their values are fixed by §11.2 and are not open for a
  source to extend: `scheme` (ocean|crimson|stone|forest|sunset|mono),
  `mode` (light|dark), `contrast` (normal|high), and the per-app
  overlay.
- `resolution_order` lists the axes in the order they layer, and the
  compiler MUST hold the source to it. An axis named in
  `resolution_order` by no `when` anywhere is **fatal**; a `when`
  naming an axis absent from `resolution_order` is **fatal**. A source
  may not claim an axis it does not implement — §1.3's
  honest-by-construction rule applied to contexts, and the reason it is
  fatal rather than a warning is that a claimed-but-unauthored axis
  resolves silently to the base values and so looks like a working
  theme.
- **A `when` MAY name a conjunction of axis values, and for the shipped
  palettes it MUST.** The 6 × 2 `scheme` × `mode` product is not
  factorable: dark `palette.background.1` is not one value across the
  six schemes, light is not one value across the two modes, and
  splitting the colour into per-component paths does not rescue it
  either. Single-axis layering therefore cannot express the palettes
  §11.2 requires be shared verbatim with the web design system, which
  is a property of the palettes rather than of any encoding chosen for
  them.
- **Precedence is by specificity, not by document order.** Every block
  selected by the compiled context applies in ascending order of its
  **specificity key** — the number of axis values its `when` names,
  then the position in `resolution_order` of the last axis it names —
  each later block overwriting only the paths it names. Two selected
  blocks with the same key writing the same token path are a **compile
  error**: the same more-specific-wins, ambiguity-is-fatal rule §4.3
  states for mappings, so the system has one specificity law and not
  two. Because ties are fatal, the sort is total and the resolved value
  never depends on where a block sits in the file.
- Modifiers compose by layering — a mode layers on a scheme, a
  scheme × mode block on both, high contrast above those, a per-app
  overlay last. Composition is total: every point of the axis product
  MUST resolve, and §3.4's gate ranges over every reachable point, not
  merely the default one.
- Only the primitive and semantic tiers are modifiable. A modifier
  block MUST NOT alter a family mapping: variant/size structure is
  Rust-owned (§4) and does not vary by context. Dark mode changes what
  a token is worth, never which variants exist.

## §2 Token tiers, pairs, and reference rules

2.1 **Two tiers, mandatory:** primitive and semantic. A component *token*
tier exists only by written exception recorded in the source (the
component **mapping** tier of §4 is not a token tier).

2.2 **Primitives** are the palette anchors and the metric scale bases.
Revision-1 colour primitives are the web-shared **role anchors** —
`palette.background.1`, `palette.background.2`, `palette.background.3`,
`palette.foreground.default`, `palette.foreground.muted`,
`palette.accent.default`, `palette.accent.hover`, the mode-dependent
`status.success`, `status.warning`, `status.danger`, and `transparent`
— authored in OKLCH (§3) and shared **verbatim** with the web design
system (§11.2). The names are a source-format wire contract on the same
footing as §2.4's semantic vocabulary: an authored theme references them
by string, so renaming one is a format break. A later source revision
MAY introduce Radix-model 12-step hue ramps with semantically assigned
step roles (1–2 backgrounds, 3–5 fills, 6–8 borders, 9–10 solids, 11–12
text); revision 1 MUST NOT synthesise unshared intermediate values,
because the web palette publishes seven role anchors per context and a
synthesised eighth is a desktop-only colour that the verbatim claim
would then silently cover.

Metric primitives comprise an **authored** spacing scale, an **authored**
type scale, and a required `px` base metric named `radius`. After
modifier flattening (§11.2) and before any `step` is resolved, the
compiler derives the `radius` scale in `sm, md, lg, xl` order as
`max(base−4, 0) / max(base−2, 0) / base / base+4` — the shipped
`RadiusScale` arithmetic, clamp included. A source MUST NOT author a
scale named `radius`, in its base or in any modifier block: authoring
both the knob and the scale is precisely the desynchronisation the
single knob exists to prevent, and a source that names both has stated
two answers with no rule to choose between them. Single-knob derivation
is normative **for radius**; it is not a general law over scales, and
the authored spacing and type scales are not defects awaiting a knob. A
scale earns a knob only by amendment to this chapter, which must state
the knob's name, the exact multipliers and their order, whether
authoring the scale becomes forbidden, and the §15 gate that holds it.

2.3 **Semantic tokens that carry text are authored as atomic pairs**:
surface and foreground declared together, repointed together, validated
together (§3.4). There is no free-standing "background" semantic token
where text can sit on it.

2.4 **The v1 semantic vocabulary is closed** and is *shadcn-derived*
(standing rule: CTK follows shadcn semantics), with two documented
deviations — `base` replaces shadcn's `background` (so every pair is
uniformly `<name>` + `<name>.foreground`), and `destructive` carries an
explicit authored foreground rather than an implied one (§2.3 admits no
foreground-less text surface):

- pairs: `base`, `card`, `popover`, `primary`, `secondary`, `muted`,
  `accent`, `destructive` (each `<name>` + `<name>.foreground`);
- **non-text colour tokens:** `border`, `input`, `ring` — lone colours
  by design, carrying no text-contrast claim. The compiler MUST reject
  a non-text token in any text-bearing position (§10.3's positional
  discipline applies to tokens, not just derivations); `ring` carries
  its own non-text contrast obligation (§3.5);
- metric bases: `radius` (the single knob, whose scale the compiler
  derives — §2.2), the authored spacing scale, the authored type scale
  (§5).

Extension is by exception process (source-recorded rationale + this
chapter amended), never ad hoc. The audio-console `ctk.*` names migrate
onto this vocabulary with an alias window through migration slice 3
(decision 4); console names survive only as mapping-internal names where
genuinely domain-specific.

2.5 **References flow strictly downward:** component mapping → semantic
→ primitive. A semantic token MUST NOT reference a mapping; a primitive
MUST NOT reference anything. §2.2's radius derivation is not a
counter-example: the generated scale's dependency on the `radius` metric
is compiler law, not an authored reference, and no source expresses it. A semantic token that merely renames one
primitive one-for-one is a lint warning (no ceremonial indirection).

2.6 Aliases resolve only after modifier-context flattening (§11.2).
Cycles are fatal.

2.7 **Every metric value carries an explicit unit.** A bare number is
**fatal** wherever a metric is authored. Three kinds ship, and the set
is closed — it extends only by amending this chapter:

- `px` — a device-independent pixel length (control heights, padding,
  border widths, radii);
- `step` — an index into a named scale (the type scale, the spacing
  scale, the derived radius scale), resolved to `px` through that scale
  at compile time;
- `ratio` — a dimensionless multiplier or fraction, such as the lift
  amounts §10's derivations consume.

Without the tag §9.6's type-preservation rule is unenforceable: an
untyped metric map cannot tell a 28 px control height from a 0.04 lift
fraction, so nothing can reject an override that swaps one for the
other, and the failure is silent — a fraction read as a length yields a
zero-height control, not an error. The compiler MUST reject a reference
that uses a metric where a different kind is expected, with one
consequence of the definitions above rather than an exception to them: a
**length** position accepts `px` or `step`, because a `step` *is* a px
length once its scale has resolved it, and a scale step is the authoring
form §9.5 asks metric overrides to prefer. Refusing it there would
forbid the scale from reaching the dimensions it exists to set. `ratio`
never satisfies a length position and no length ever satisfies a
position that requires a `step` — those are the confusions the closed
set is for. The compiler MUST also reject
an override whose replacement carries a different kind from the value
it overrides. The resolved artifact stores px; provenance records the
step or ratio the px came from, so §14 introspection can still name the
authored quantity.

A **source number denotes its parsed double**, not its decimal lexeme.
Every rule in this specification that constrains a numeric value —
non-negative, finite, integral, within a bound — is a rule about the
double the source parsed to, and a compiler MUST NOT be expected to
distinguish two lexemes that parse to it alike. So `5.0000000000000001`
in a step index *is* the index 5 and is accepted; `-1e-400` *is* zero
and satisfies non-negative; and any decimal at or above 2^53 is refused
outright, because above that bound a double no longer separates
consecutive integers and the compiler cannot know which index was
written. That last one is a refusal to guess, not a judgement that the
index is implausible.

This is what the substrate's strict-data format already does — `*.mix`
numbers are doubles and nothing downstream retains the lexeme — so the
rule is written down rather than introduced. The alternative, carrying
the authored spelling through parsing so the compiler could reject a
number that means exactly what a legal number means, would buy a
diagnostic nobody wants at the cost of a numeric tower the rest of the
substrate does not have.

## §3 Colour policy

3.1 **Authoring space is OKLCH** with explicit colour-space annotation.
Hex is never a v1 source value (v0 exempt per §1.5). Conversion to each
delivery space happens per emitter; truth is stored once, in author
space.

3.2 **Evaluation space is the delivery space.** Contrast is measured
after conversion and gamut-mapping into sRGB, in that defined order —
never in author space. Gamut-mapping happens before any contrast
measurement so the gate sees what the screen shows.

3.3 **Compositing honesty:** a text-bearing pair MUST be opaque or MUST
declare the backdrop it composites over; the declared composite is what
the contrast gate measures. Translucency MUST NOT quietly lower rendered
contrast below the measured value. Intentional-alpha tokens (scrims)
remain legal where no text contrast claim attaches.

3.4 **The WCAG gate is a compile gate.** Every authored pair, every
reachable cell, and every **admitted** member of the override product
(§9.4) **whose output is a text-bearing pair** MUST pass AA at compile
time. That qualifier is not a loophole: §9.4's product walk is total
over admitted substitutions, and each member is held to the
postcondition its output kind declares, so a non-text member excused
here is a member §9.4 sends to §3.5's 3:1 gate instead — not one that
escapes. Text AA is the wrong measure for a focus ring, and a clause
that demanded it of one would be failed by every conforming
implementation. Failure is fatal per §11.5 — there
is no silent substitution; the diagnostic *suggests* the guaranteed-AA
black/white pair, and a human or agent applies it to the source. A
declared §9.3 exclusion is not a failed contrast check, because the
resolver cannot select it.

3.5 **Non-text contrast (the focus indicator is load-bearing).** §3.4
measures text pairs; `border`, `input`, and `ring` carry no text and
need a different gate. `ring` declares its **adjacency set** in the
source — the pairs whose surfaces it is drawn against — and MUST meet
WCAG 1.4.11 non-text contrast (≥ 3:1) against every declared adjacent
surface, measured in the same §3.2 pipeline; failure is **fatal** (an
invisible focus indicator is an accessibility failure, not a taste
choice). A family mapping using `ring` on a surface outside its
declared adjacency set is a compile error. `border` and `input` SHOULD
meet the same 3:1 against their adjacent surfaces; shortfall is a
**warning** (decorative borders are legitimate; invisible focus is
not).

3.5.1 **Adjacency is scoped to the authored mapping, not the override
product.** Ranging ring adjacency over every `PairRef` a §9 override
could name would force every adjacency set to be universal, at which
point declaring one carries no information. The rule runs the other way
instead: the compiler checks `ring` against the surfaces the authored
mapping actually draws it on, and treats a surface override landing on
a ring-bearing cell as the checked object — an override that would
place `ring` on a surface outside its declared adjacency set is
rejected, at the same compile time and with the same fatality.
Reachable coverage is identical either way; the difference is that the
diagnostic points at the override that broke the contract rather than
at an adjacency set no author could satisfy.

3.5.2 **Derived focus indicators.** One lone `ring` value cannot satisfy
§3.5 across the shipped contexts. A button family draws focus on at
least four surfaces — secondary/control, primary, destructive, and the
muted composite — and in every dark context the luminance window that
clears 3:1 against the near-black background is disjoint from the one
that clears it against the light accent: the requirement is infeasible,
not merely unauthored, and no choice of hue changes that because
contrast depends only on luminance. A family mapping's ring property
therefore MAY name a registered **non-text derivation** —
`focus_ring(colour, pair)`, the seed and the surface — in place of a
lone token. The seed is an **explicit typed argument** (§10.4),
conventionally the `ring` token itself, and deliberately not a hidden
read of it: §10.5 admits exactly one compiler-supplied implicit input
(`mode`), and a derivation whose result depended on an undeclared token
could not be re-executed from the retained recipe under §10.2, because
the resolver would have to consult a dependency the recipe never
recorded. Naming the seed also puts it where an agent reading the
mapping can see what the indicator walks away from. Such a derivation
carries a **non-text contrast postcondition**: ≥ 3:1 against the
resolved surface of the pair it is called with, measured in the §3.2
pipeline, evaluated eagerly into its cell per §10.2. A derived ring
declares no adjacency set; the postcondition is its obligation,
discharged per cell where the value is produced rather than per
declaration. Lone `ring` tokens remain legal and remain governed by
§3.5 and §3.5.1.

**The pair is the sole substitutable slot; the seed is fixed.** §10.2
allows a signature zero or one substitutable slot, and for `focus_ring`
that slot MUST be the pair. The two wrong answers are both reachable by
a legal registry entry and both silently wrong. Declaring *no* slot
means an override that moves a cell from `secondary` to `primary` does
not re-execute the ring: the indicator stays the one computed against
`secondary`, is never re-checked, and can sit below 3:1 on the surface
actually painted. Declaring the *seed* as the slot lets an override
repoint what the indicator is derived from while leaving the surface it
must contrast with untouched, which inverts the derivation's purpose.
The seed is therefore a retained fixed binding, not a substitution
target. §9.3 classifies every pair as admitted or excluded for that
slot; §9.4 then evaluates every admitted substitution and holds it to
the postcondition this derivation's output kind declares — the non-text
3:1 one, not the text one, which is why §9.4 names the postcondition by
output kind rather than fixing it at text.

**The observable is distance from the authored seed**, not from the
accent as such. `colour` is a legal argument kind and any opaque seed
preserves totality, so `focus_ring(status.danger, primary)` is
well-formed and is not restricted here; the convention of seeding from
`ring` is what makes the indicator read as the brand accent, and it is
a convention rather than a rule. The provenance therefore **names the
seed it walked from** alongside the distance, so a distance figure is
never silently attributed to a token that was not the seed.

3.5.2.1 **The walk carries no distance cap, and the gate is on
distance.** `focus_ring` moves its seed in **lightness only**, holding
chroma and hue at their nominal values so a ring seeded from the accent
still reads as the accent. The walk takes the **nearer** of the two
directions and carries **no cap of its own** — it is bounded only by
the lightness interval itself, which is where the endpoints that make
it total live.

**Chroma is nominal, not delivered.** Holding chroma while lightness
saturates would leave sRGB, so §3.2's conversion gamut-maps by reducing
chroma toward zero until the colour is representable — and the endpoints
of the walk are therefore achromatic, not saturated. This is stated
because the totality argument depends on it: an implementation that
genuinely fixed delivered chroma could not reach the endpoints and would
not be total. Identity is preserved where the gamut permits it and
surrendered where it does not, which is the correct order of priority
for a focus indicator.

**The seed MUST be opaque — a precondition — and the output opaque as
an invariant.** Totality is a claim about the ring's own luminance, and
a translucent ring has none of its own: at alpha 0.5 over a surface of
luminance 0.5, white composites to 0.75 and black to 0.25, giving
1.45:1 and 1.83:1 — both directions fail, and no walk recovers them.
§3.3's compositing honesty governs text pairs; this is its non-text
counterpart. The two are not the same kind of obligation, and saying so
matters: seed opacity is a genuine **precondition** on an authored
value, which an authored palette can violate and which no amount of
walking can discharge; output opacity is an **invariant** that a
lightness-only walk from an opaque seed cannot break, so violating it
takes an evaluator defect, not a palette.

The registry **declares** both; it cannot check either there.
Registration sees a signature, not a value: the seed is a context-
resolved colour that differs across the twelve contexts and after any
modifier resolution, and the output does not exist until the derivation
runs. Enforcement is therefore **per cell**, on §10.7's terms — seed
opacity checked at each call once the seed has resolved, output opacity
checked after every execution, eager or override-triggered, and a
violation fatal *at the cell that provoked it*, naming the cell. This
is the same lifecycle §10.7 already requires for the postcondition, and
the output check earns its place the same way that section's does: an
invariant nothing verifies is documentation.

Under those two conditions the derivation is **total** and the
postcondition unfailable: white falls below 3:1 only against a surface
whose relative luminance exceeds 0.30, and black only below 0.10. Those
conditions are disjoint, so every surface is served by at least one
direction, and no reachable cell can fail. §3.5's fatality is
**discharged by construction** for a derived ring, not relocated to it;
it continues to govern lone `ring` tokens, which are authored values
with no walk to save them. Discharged by construction is not unchecked:
§10.7 requires a derivation that meets its postcondition by construction
to be verified against its actual output anyway, and that check is what
§15's negative fixture exists to prove.

**The walk is specified tightly enough to be reimplementable.** In each
direction the candidates are, in order, `seed_L ± n/1000` for
n = 0, 1, 2, … while the result stays inside the closed interval
[0, 1], **followed by that direction's endpoint** — 1.0 going up, 0.0
going down — as a final terminal candidate whenever the lattice has not
already landed on it exactly. The endpoint is not an optional extra: a
seed at L = 0.4237 walks up through 0.9997 and no further, so without
the terminal candidate the discrete search never evaluates white and
the endpoint proof above would be a claim about a continuum the
implementation does not search. The terminal candidate takes step index
N+1, where N is the last lattice step inside the interval, which is
also its true ordering — it is strictly further from the seed than any
lattice candidate in that direction.

Ordering, ties, and the threshold are all compared in **integer step
units**, never on subtracted `f64` lightnesses. The result is the
**nearest passing** candidate: the lowest step index at which the
composited ratio reaches 3:1, with the ratio compared at or above 3.0
exactly and no tolerance band. When both directions first pass at the
same step index — reachable whenever the seed sits symmetrically
between two qualifying bands — the **lighter** candidate wins. The
recorded distance is that integer step count, so "300 is silent, 301
warns" is a decision about two integers and cannot drift with floating-
point representation. Every part of the choice is thereby a function of
the inputs rather than of iteration order or arithmetic order.

What a bound was protecting is preserved as an **observable** instead.
Every derived ring records **two distinct numbers** in its cell's
provenance, and conflating them is how a reader would later mistake an
ordinal for a measurement: `step_index`, the integer step count that
every decision above is made on, and `delta_l`, the actual lightness
travelled. They are not the same figure — a seed at L = 0.4237 reaching
the upper endpoint has `step_index` 577 and `delta_l` 0.5763 — and the
gate fires on `step_index` alone. Where this section quotes distances
in lightness (0.057, 0.437, …) it is quoting `delta_l`, because that is
the figure a palette author reasons about; the compiler compares
integers. A `step_index` strictly above the threshold raises a
**warning**, never an error: a
ring that must travel far from its seed is a fact about the palette —
that pair's surface sits in the seed's own luminance band, which for
revision-1's ring-seeded calls means the accent's — and the
honest response is to say so, not to refuse to compile. The threshold is
**300 steps** (0.30 in lightness). It was **chosen from measurement**
rather than picked, and the measurement is this: across the ninety-six
pair-context cells of revision-1, seventy-two need
no walk at all, twenty-one land between 0.057 and 0.30, and exactly
three exceed it — `mono`/dark/destructive at 0.437, `stone`/dark/
destructive at 0.385, and `ocean`/dark/destructive at 0.342. Only
`primary` and `destructive` ever require a walk, which is §3.5.2's own
prediction confirmed: they are the saturated surfaces that sit closest
to the accent in luminance. A threshold that fires on those three and
stays silent on the twenty-one routine walks reports outliers rather
than narrating the normal case.

The same measurement retires the bounded alternative on evidence: a
±0.25 bound would have been **fatal on eighteen of the ninety-six
cells**, including `primary` and `destructive` in nearly every context.
It was never satisfiable by the shipped palette.

Being chosen, 0.30 is a **calibration against revision-1**, not an
invariant of the algorithm: a later palette that legitimately walks
further will fire it, and the correct response then is to re-measure and
re-state the threshold with its evidence, exactly as this one is stated.
The threshold's own behaviour is therefore tested synthetically, at and
just above the boundary, and not only through the revision-1 counts
(§15) — a test that asserts "three cells warn" is a regression test for
the palette; a test that asserts "0.300 is silent and 0.301 warns" is a
test of the gate.

## §4 Component mapping algebra

4.1 Per family, the source declares a cva-shaped table:
`base + variants{variant, size} + states + compoundVariants`. Values are
token references, pair references, and registered-derivation calls
(§10) only.

4.2 **What data MUST NOT express:** ECS component names, entity
selectors, arbitrary expressions, or variant axes not declared in the
family's schema (§8.2). Data chooses values; Rust owns structure.

4.3 **Specificity, degenerate and closed:** within a table,
more-non-default-axes wins (Spectrum's count rule). Two rules of equal
specificity setting the same property for any reachable cell is a
**compile error** — ambiguity is never an iteration-order outcome.

4.4 **Mandatory family base:** every resolver-owned presentation
property of a family MUST have a `.mix`-authored base value; a family
declaration that omits one is a compile error. Rust spawn-time component
values are transient placeholders that exist only until the resolver's
first pass — they are never a styling authority and never part of any
fallback chain.

4.5 **Explicit `null`** participates in the specificity/conflict check
and means *revert to the family base*. On a transition into a null cell
the resolver **writes** the base value — it never skips the write and
never removes a component. A cell with no matching rule takes the base
the same way.

4.6 **Totality vs coverage.** Lookups are total by construction
(enum-indexed tables over the shared family schema, §8.2): a new enum
variant's cells fill from base and are never a runtime hole. That fill
MUST NOT be silent: the compiler emits `new-variant-uncovered` for any
axis value with no authored rows — a warning by default, **fatal** for a
family declaring `coverage: explicit`. Discharge is authoring the rows
or declaring `inherit: base` on the axis value; either way the fill is a
written decision.

## §5 Typography

5.1 The artifact carries a resolved family/weight/size scale **plus** a
compiled `family × part × (variant × size)` → typed type-record
assignment table. The record (`{type_step, weight, …}`) holds everything
the mapping may author for a text part — the IR is never narrower than
the authoring contract.

5.2 Text-part typography is authored in the family mapping under §4's
rules (base / variants / null / coverage). No Rust match arm carries a
size.

5.3 **Typography is barred from state:** a `states` or
`compoundVariants` rule that sets a typography property is a **compile
error**. Font changes on interaction reflow text; state may change text
*colour* only (family-resolver-owned). The assignment table's missing
state axis is a rule, not a gap.

5.4 **One `TextFont` writer.** The typography resolver (the evolved
`apply_ctk_typography`) is the registry's sole owner of `TextFont`
across managed text. The shipped second writer
(`reconcile_button_label_fonts`) is absorbed: per-part sizing becomes
the typography resolver's job, keyed by part marker + owning
family/size context, resolved against §5.1's table. Its discipline
survives internally: part sizing applies after generic family
resolution; writes stay change-tick honest.

5.5 The bundled-font + glyph-coverage contract of the 2026-07-22 ADR
carries forward. Fonts cross the feathers seam as asset handles, never
tokens (§13.1).

## §6 State model

6.1 One generic normaliser system writes one component:

```rust
CtkStyleState {
    interaction: Resting | Hovered | Pressed | Disabled,
                        // precedence: Disabled > Pressed > Hovered > Resting
    checked: bool,      // orthogonal axes, not extra enum arms
    selected: bool,
    focus_visible: bool,
}
```

6.2 The normaliser **never paints**. It reads modern state components
only — never legacy `Interaction` — ending dual-input ambiguity.

6.3 **Focus is resource-driven:** visible focus is computed from
`InputFocus` + `InputFocusVisible`. On resource change the normaliser
rewrites `focus_visible` on **both** the previously and newly focused
entities (it tracks the prior holder; a component-only contract would
leave one entity stale).

6.4 **Removal-aware:** the inputs it summarises are frequently removed,
not changed (`Pressed`/`Checked`/`InteractionDisabled` markers). Its
triggers MUST include removal observation (`On<Remove>` /
`RemovedComponents`), not `Changed<>` alone.

6.5 Resolvers take state as an **argument** into the family table. There
is no state selector, no pseudo-class, no per-state component insertion.
The state enum is closed in Rust; data can map styling *for* a state,
never invent one.

## §7 Invalidation matrix

A family resolver MUST repaint on every row; each row carries a named
regression test:

| Trigger | Contract |
|---|---|
| `Changed<CtkStyleState>` | repaint the widget's cells |
| Owning-family axis change (variant/size component) | dirties the family resolver **and**, for text parts, the typography resolver (the Medium→Large label case) |
| Override component added / changed / **removed** | removal restores table values (feathers' removal trap, owned) |
| Focus-resource change | via the normaliser's two-entity rewrite (§6.3) |
| Part entity inserted late | `On<Insert>` on part markers — an asynchronously materialised label styles itself on arrival |
| Reparenting / relationship change | re-resolve membership-derived styling |
| `ResolvedDesign` revision bump | repaint all managed entities |

**Cost model (normative honesty):** settled widgets incur no writes and
no text-rerender re-entry (change-tick honesty:
`bypass_change_detection()` + `set_changed()` only on real writes).
`Changed<>` filtering still scans matching entities each frame — the
floor is a change-tick scan, not zero. The sanctioned escape hatch, if
profiling ever demands one, is observers/an explicit dirty queue — not a
new architecture.

## §8 Ownership

8.1 **Single-writer is audited policy, not structure** — Bevy cannot
make `BackgroundColor` privately writable. Three mechanisms of stated
strength enforce it:

1. **The ownership registry (normative):** a table of
   `family × part × component → owning resolver`, covering CTK and
   app-defined families alike. Every present-day styling writer is
   either absorbed into a family resolver or listed with an explicit
   boundary (the typography resolver, §5.4). App presentation writers
   (filemgr's browser painter, studio's settings styler, and peers)
   register as app family resolvers under the same rules and audit.
2. **Writer-attribution audit (the enforcement):** per-writer mutation
   counters + schedule-boundary sentinels assert, over scripted
   interaction scenarios, that only the owning resolver's counter moves
   for registry-owned components. Change ticks and final values cannot
   attribute writers; counters can.
3. **Removal test (necessity only):** removing a family's resolver MUST
   leave its entities visibly unstyled at rest. This proves
   load-bearing, not uniqueness — uniqueness is the audit's job.

8.2 **Family schemas are centrally registered:** variant/size enums and
part sets are defined once in the shared headless compiler crate
(§11.1); ctk and apps depend on it, never the reverse. Desktop-wide
scope therefore makes that crate a **closed central family-schema
registry** — an app adding a private family edits the shared crate
(decision 8; an app-owned type-keyed extension mechanism preserving
totality is deferred follow-up, §15).

8.3 **Multi-entity widgets use part markers, not selectors:**
`ButtonRoot`/`ButtonLabel`, `MeterChrome`/`MeterFillProcedural`.
Membership is a component relationship, never a tree query by name.

8.4 **Procedural boundary is entity-structural:** value-driven geometry
(meter lanes, knob rotation, waveforms) lives on procedural entities
(widget-owned, resolver-forbidden) split from chrome entities
(resolver-owned). Both are listed in the registry.

## §9 Overrides

9.1 Overrides are **closed, typed, per-family components**,
Option-per-field (e.g. `ButtonStyleOverride { min_width:
Option<MetricRef>, …, surface: Option<PairRef> }`). No open property
bag, no stringly-typed keys.

9.2 **Two-step chain, nothing else:** `instance override ?? resolved
table`, per property, at resolver time, against the artifact's
dictionary. `None` means the table's value. No ancestor lookup, no
specificity, no stacking of override components. Per-app variation is a
compile-time overlay (§11.2), never a third runtime link.

9.3 **Contrast-bearing overrides repoint domain-constrained pairs**,
never lone colours: a surface override names a `PairRef` from the
closed pair set, moving background and foreground together. Where the
target cell retains a recipe with an override-substitutable slot, the
resolved artifact MUST classify every member of §2.4's pair set as
either **admitted** or **excluded** for that slot in that resolved
context. An exclusion MUST carry the registered domain constraint that
caused it. The resolver MUST refuse an excluded `PairRef` before recipe
execution and MUST NOT silently substitute the cell's eager value.
Where no substitutable slot exists, the override replaces the computed
pair whole under §9.2; every resolved pair is admitted by the text
gate.

9.4 **Exhaustive permitted-product validation:** the pair set, cells,
and recipes are finite, so the compiler MUST contrast-check the whole
**admitted** `PairRef × cell × recipe` product at compile time. The
authored binding of a recipe MUST itself be admitted. Every admitted
substitution MUST evaluate successfully and satisfy **the postcondition
its own output kind declares** — the §3.4 text-contrast postcondition
for a pair-output recipe, the §3.5.2 non-text 3:1 postcondition for a
non-text-output one. Failure is fatal. Naming the postcondition by the
recipe's output kind rather than fixing it at "text" is load-bearing: a
derived ring's whole obligation is to contrast with the surface
*actually painted*, and a product walk that checked only text
postconditions would walk right past an admitted override that repoints
a ring's surface. A declared domain
exclusion is neither an evaluation failure nor a diagnostic: it is
availability data retained under §9.3. The classification MUST be total
over the closed pair set. No runtime combination **permitted by the
artifact** may exist that the WCAG gate never saw.

§3.5.1's override-scoping is an exception to this product walk, and it
is narrow: it applies to a **lone adjacency-bearing `ring` token**,
where checking the override rather than the product reaches the same
combinations without collapsing every adjacency set to universal. It
does not apply to a derived ring, which declares no adjacency set
(§3.5.2) and is therefore covered by the product walk above like any
other admitted substitution.

The totality this replaces was unsatisfiable as written, and the
distinction it now draws is the whole reason. A surface-moving recipe
can never change a fully transparent surface's delivered bytes, in any
palette, by construction — so a dictionary containing a transparent
pair could not satisfy the old clause at all. The shipped dictionary
contains one (`muted`), and the Ghost variant's resting identity *is*
that transparency, so the choice was never between a strict rule and a
lax one: it was between dropping transparent pairs from the vocabulary
and degrading the failure to a warning nobody reads. Making
unavailability first-class and machine-readable keeps AA totality of
*meaning* — every combination the artifact permits was checked — while
letting a slot's admissible subset be smaller than the closed set, and
saying so in the artifact rather than in a diagnostic beside it.

9.5 **Metric overrides reference the scale** (`MetricRef` — a scale step
or metric token); every rendered dimension stays reconstructible from
source. **Declared exception (decision 6):** raw-pixel instance geometry
is allowed only through explicit sized-constructor APIs and is stamped
`entity-local` in provenance.

9.6 **Type preservation (Spectrum's rule):** an override MUST NOT change
the value type of what it overrides.

## §10 Derivations

10.1 Data may **call** registered derivation functions by name with
token arguments (`derive: contrast_safe_lift(background, text.dim)`);
it may never define expressions. Membership of the registry is an
implementation matter, governed by rules rather than by a list here:
every registered function MUST be deterministic, testable in isolation,
and declare its postconditions (§10.3), its substitutable slot (§10.2),
and that slot's substitution domain (§9.3) as data the compiler can
read. This chapter names individual derivations only as examples; the
registry is the crate's, and an enumeration in normative text would be
stale the first time one is added.

10.2 **Eager evaluation, mechanical split:** state is a finite axis, so
every derivation evaluates at compile time into its cell for the
no-override path. Each derived cell retains its **recipe** — registered
fn + typed input bindings marking its **zero or one**
override-substitutable slot + that slot's **substitution domain** +
output property. The family resolver re-executes a recipe only when an
instance override repoints that slot; the override operand is the
*only* dynamic input in the system. Resolver-time execution never
consults raw source.

The retained recipe also carries §9.3's complete pair classification
for its substitutable slot. The resolver MUST consult that
classification before re-execution and MUST NOT reconstruct it from
registry metadata or raw source. Two computations of one rule drift;
the compiler's product walk and the resolver's availability query MUST
therefore read the same stored classification rather than each deriving
it from the domain constraint.

A recipe with **no** substitutable slot is one whose every input is
fixed by the compile-time context — it is evaluated once and never
re-executed, and its eager output is the no-override table value. This
does not place the cell beyond override: §9.2's direct property
override still replaces the whole computed value, exactly as it
replaces an authored one. The slot marks *re-execution*, not
*mutability*, and the two must not be conflated — a derivation from
primitives has nothing an override could repoint, which is a statement
about its inputs, not a promise about its output. Where a slot is
present it MUST be an authored argument (never an implicit input,
§10.5), MUST be in range, and MUST be the only one.

10.3 **Contrast is never a runtime outcome:** a property in a
**text-bearing** contrast role accepts only pair refs or derivations
carrying a text-contrast postcondition. A derivation that carries no
such postcondition MUST be rejected by the compiler in those positions
— whether it is unregistered, or registered for use elsewhere and
simply makes no contrast claim (an unchecked lift is the obvious case)
— as are the non-text tokens (§2.4). Both arms are obligations: an
unknown name and a known name without the claim fail alike, and neither
may be waved through on the grounds that the author probably meant
something safe. A property in a
**non-text** contrast role (§3.5) accepts a lone non-text token or a
derivation carrying the **non-text** postcondition of §3.5.2; a
derivation with no postcondition is rejected there for the same reason
it is rejected in a text position. The two postconditions are distinct
obligations and neither substitutes for the other: 3:1 against one
surface is not the AA claim, and an AA pair says nothing about a ring
drawn beside it. §9.4's product validation covers **admitted**
override-substituted executions.

10.4 **Arguments are typed at the authoring site.** §10.1's
`contrast_safe_lift(background, text.dim)` is a display form; the v1
source encoding is a list of tagged arguments, each naming its kind —
`pair`, `colour`, `colour_list`, `ratio` — alongside the token path or
paths it references. A bare string cannot carry this: `"primary"` is a
legal pair name, a legal colour name, and a legal one-element list, and
the position it sits in is not enough to disambiguate a registry whose
rows differ only in argument kind. Tagging the argument makes the
authored intent checkable against the signature rather than inferred
from it, and makes a mis-typed call a diagnostic at the call rather
than a surprising value downstream. Arity and kind are both checked
against the registered signature; neither is negotiable, and a
positional mismatch is fatal.

10.5 **Implicit inputs are supplied by the compiler, not authored.** A
derivation MAY declare inputs that the compile-time context already
determines — presently the context's `mode` (§11.2). These are not
arguments: they are not written at the call site, do not count toward
its arity, and can never be the substitutable slot, because there is no
authored operand for an override to repoint. They ARE part of the
retained recipe: §10.2 requires resolver-time re-execution to consult
no raw source, so a recipe whose result depends on mode must carry the
mode it was compiled under. The alternative — re-deriving the context
at resolve time — would make a resolved artifact's meaning depend on
where it is read, which §11.2's two-stage split exists to prevent.

10.6 **Derivations are legal wherever data supplies a value**, which
includes the semantic tier (§2.3's atomic pairs) and not only family
mapping cells. §10.1 already implies this — its own example calls a
derivation on semantic and primitive tokens rather than on a cell — but
the consequence is worth stating: a derivation used only in a mapping
computes that cell while leaving the semantic token it stands for
pointing at whatever was authored, so a `PairRef` naming that token and
the cell that "is" it resolve to two different colours. A derived
semantic pair is validated exactly as an authored one (§3.4), and
remains a legal `PairRef` target and override operand: the tier is
defined by what a token *is*, not by how its value was arrived at.

10.7 **Evaluation is per cell, and may fail.** §10.2's eager evaluation
happens where the value is *produced* — the cell — not where the rule
that supplied it is compiled. The distinction is invisible while every
derivation is total and becomes load-bearing the moment one is not: a
recipe evaluated once per rule and cloned into every matching cell can
neither discharge a per-cell postcondition (§3.5.2) nor name the cell
in its diagnostic. A registered derivation MAY therefore fail, and a
failure is fatal at the cell that provoked it. A derivation that meets
its postcondition by construction is checked anyway, against its actual
output: a registry that asserts a postcondition it never verifies is
documentation, and the whole point of §10.3's typing is that the
assertion is load-bearing.

Per cell is where evaluation *happens*; it is not a licence to report
the same fact ninety-six times. Two evaluations whose recipe and whose
every bound input are identical have one outcome by determinism
(§10.1), so the compiler MUST evaluate them once and report a failure
once. This is not diagnostic de-duplication after the fact — the
identity is in the inputs, so a recipe that genuinely varies per cell
has distinct inputs and keeps its distinct per-cell diagnostics. A
compiler that emitted one line per cell for a fault that has one cause
would be reporting the shape of its own loop.

10.8 **Derivations read delivered colours, not authored anchors.** A
recipe's colour inputs are the resolved primitives as they will ship —
after §2.1's gamut mapping — never the pre-mapping authored OKLCH. The
reason is §3: every contrast gate in this spec measures delivered
colours, so a derivation seeded from a colour no display can show could
not have its postcondition meaningfully verified, and the surface it
computed would be related to nothing the user sees. The consequence is
real and MUST NOT be papered over: for an authored anchor outside sRGB,
a chroma-proportional derivation yields less chroma than the same
arithmetic applied to the authored value, because the proportion is
taken of the reduced chroma. That is the intended reading — the anchor
that survived is the anchor. A future wide-gamut output space changes
what gamut mapping does, not this rule.

A **pair** argument denotes the complete resolved pair record — its
surface and foreground, its declared backdrop (§3.3), and the delivered
composites measured from them. Those values are fixed inputs of the
retained recipe in that artifact revision; they are not independent
runtime operands. A `PairRef` therefore selects a pair *together with*
its backdrop, which is why §9.4's product ranges over pairs and not
over pair × backdrop: a modifier that moves a backdrop produces a
different resolved context, and §11's compile rebuilds every reachable
context's pairs, recipe outputs, and availability classification
together. This matters for a recipe whose delivered movement is in the
foreground over a transparent surface, where the contrast claim is
measured against the backdrop composite: the dependency is real, and it
is discharged by the artifact being rebuilt as a whole rather than by
the recipe naming the backdrop a second time.

## §11 Compile and apply

11.1 The compiler is a **dependency-light headless crate** — no Bevy, no
ctk; ctk depends on it. It also hosts the family schemas (§8.2).

11.2 **Two-stage resolution, defined once:**

1. *Compile time:* flatten modifier contexts in declared
   `resolution_order` over the shipped axes — `scheme`
   ocean|crimson|stone|forest|sunset|mono (the six palettes, shared
   verbatim with the web design system), `mode` light|dark, `contrast`
   normal|high, per-app overlay — DTCG-resolver-shaped; contexts
   compose by §1.6's specificity key (a mode layers on a scheme, a
   scheme × mode block on both, high-contrast above those); apply
   per-app overlays as **compile-time inputs** (visible in every cell
   and diagnostic — never a runtime fallback layer); resolve aliases
   only after flattening; generate §2.2's derived scales from the
   flattened metric bases, then resolve every `step` through the
   combined scale set; evaluate every derivation eagerly into its
   cell.
2. *Resolver time:* table lookup; recipe re-execution only under an
   instance override (§10.2), against the artifact's dictionary.

11.3 The artifact:

```rust
ResolvedDesign {
    revision: u64,     // monotonic; every successful apply bumps it
    tables,            // per-family variant×size×state cells (linear RGB, px)
    dictionary,        // resolved token + pair map — override authority
    recipes,           // per-cell derivation recipes (§10.2)
    typography,        // scale + family×part×(variant×size) → type-record (§5)
    provenance,        // per value: token path + producing rule
}
```

11.4 Validation is total at compile time: unknown refs, type mismatches,
contrast failures, mapping ambiguities, coverage diagnostics, cycles —
all before anything touches the ECS.

11.5 **Failure semantics — one model:** outcomes are **fatal** (artifact
not produced) or **warning** (artifact produced, diagnostic attached).
Diagnostics live in a separate `DesignCompileStatus` resource (source
identity, outcome, diagnostics, timestamp) — never inside the artifact —
so a failed compile is fully reportable while the previous
`ResolvedDesign` stays live (**last-known-good**).

11.6 **Boot order:** an embedded default source, compiled at build time
and proven valid by a unit test, always produces revision 1; user/app
sources compile on top. A broken user file on first launch renders the
default theme and reports the failure — never unstyled, never a crash,
never fuchsia.

11.7 **Apply is an atomic resource swap** of the whole artifact.

## §12 Wake and reload

12.1 Source **content** mutation flows through authoritative paths.
None ships today: the shipped theme editor writes *selection only*
(`ThemeWriteRequest` persists scheme + mode — §12.3's other lane), not
source content. Content-mutation paths arrive with this chapter
(authoring tooling; a `designd` verb later), and whatever mutates the
source MUST publish a **design-source-changed** wake on the bus,
debounced/coalesced.

12.2 The wake triggers `compile → swap → resolver pass` in one defined
schedule order.

12.3 Selection wakes and content wakes are distinct lanes and stay
distinct. The v1 selection is **scheme + mode + contrast** — all three
§11.2 user axes; a change to any of them is a selection change and
triggers §12.2's pass for the newly selected context. The shipped
legacy `ThemeChanged` (scheme + mode only) implies `contrast: normal`
for the length of the v0 window; the wake **and the persisted
selection** grow the contrast field together in migration slice 4 — a
selection lane that persists fewer axes than it applies is a defect: a
live contrast choice MUST survive restart and focus-gained re-read. An
already-persisted two-axis record (today's writer has produced them)
MUST load as `contrast: normal` — never rejected, never left implying
a stale prior contrast — on both startup and focus-gained re-read.
Focus-gained re-read remains the lazy backstop for a missed wake.

12.4 Sub-minute polling is banned (standing no-poll law). There is no
file-watcher requirement; the wake is the mechanism.

## §13 Adapters

13.1 **Feathers adapter — one-way:** `ResolvedDesign` → `UiTheme` token
map (`cosmix.*`-prefixed keys coexisting with upstream's), never read
back. Fonts cross as asset handles per §5.5. Vendored feathers widgets
migrate their markers to family resolvers at vendoring time.

13.2 **Web emitter — shared context only:** emitted at deploy time as
CSS custom properties with live `var()` links (outputReferences) and
per scheme×mode×contrast permutation wrappers, written atomically, invoked
only by the web deploy path. The emitter API accepts **only the named
shared-context artifact** — per-app overlays are structurally absent
from its input.

13.3 **Web tier scope (decision 5):** primitive tier + colour method +
scheme structure only. Semantic-tier emission requires amending the
2026-07-22 ADR first. Component mappings are toolkit business and are
never emitted.

13.4 **Drift test:** both adapters are validated against the same
shared-context artifact; divergence is a test failure.

## §14 Introspection

14.1 **`AppliedStyleStamp`** on every resolver-styled entity: family,
variant, size, state, and the `ResolvedDesign.revision` it was painted
from.

14.2 **`StyleTrace`** (dev/debug): for any entity + property, the full
chain — table cell → tokens/pair → derivation → override — from
provenance. "Why is this pixel this colour" is a lookup, not
archaeology.

14.3 `DesignCompileStatus` and provenance are queryable structured data.
A `designd` ABP verb surface over the source (query tokens as data,
property writes → source-changed wake, regenerate emitters) is the
natural follow-on: deferred, but the compiler's data model MUST NOT
preclude it.

## §15 Conformance gates, deferrals, and migration

**Named gates** (each MUST exist as a test or compiler rejection before
the relevant slice ships): §1.2 versioned-section isolation (a v0
reader demonstrably never parses v1 sections) **and** schema-version
rejection — fatal for missing, non-integer, or any integer outside the
reader's supported-version set (v0 is the *absence* of the field, never
`0`); of these, only versions above the reader's maximum are classified
and reported as §1.3's version gap, and none is ever interpreted under
older rules; §1.4 equivalence gate
(incl. crosswalk completeness + absence rule); §1.6 modifier-block
rejections (an axis in `resolution_order` named by no `when`; a `when`
naming an axis absent from `resolution_order`; a block altering a
family mapping) plus a total-composition test covering every point of
the axis product, a **precedence** test (a compound `when` beats each
single-axis block it subsumes, and the resolved value is invariant
under reordering the blocks in the file) and a `modifier-conflict`
rejection (two selected blocks of equal specificity key writing one
token path); §2.2 `web-anchor-verbatim` — the compiled anchors of all
twelve contexts equal the web palette; the compiler crate cannot see
the web table, so this gate lives where both sides are visible (the ctk
port, slice 1), and until it exists the transcription is unproven, not
merely untested; §2.2 `radius-single-knob` — the derived scale equals
`max(base−4, 0) / max(base−2, 0) / base / base+4` at the shipped base,
**both** clamps proven by a base low enough to drive `sm` and `md`
negative (a single clamped entry leaves the other arm untested), a
missing `radius` base rejected, a non-`px` `radius` base rejected, an
authored `radius` scale rejected in base *and* in a modifier block, the
scale regenerated from the winning value when a modifier overrides the
knob, and the generated entries' provenance naming the generator and
the source path of that winning base rather than a fabricated authored
path; §2.4/§10.3 non-text-token
positional rejection; §2.5 reference-direction rejection; §2.7
metric-unit rejection (a bare number; a metric used where another kind
is expected; an override changing a metric's kind); §3.3
compositing-declaration rejection (undeclared translucency under text);
§3.4 WCAG compile gate; §3.5 ring non-text contrast gate (+
adjacency-set rejection), with §3.5.1's scoping proven by a fixture in
which an override moves `ring` onto a surface outside the declared
adjacency set and is rejected *as an override*; §3.5.2's postcondition
proven by a derived ring meeting 3:1 on **every** reachable cell it is
called on, across all twelve contexts, **and** by a negative fixture in
which a ring value that misses 3:1 is rejected. The walk being total
(§3.5.2.1) means the positive arm can never fail on a legal input — but
that is precisely why the negative arm is required: §10.7 obliges the
compiler to *check* a by-construction postcondition against actual
output, and an implementation that omitted the check entirely would pass
every positive fixture. The negative fixture proves the checker exists;
it does not claim the failure is reachable by an authored palette. Its
two opacity siblings are of **different kinds**, and §15 says which is
which rather than flattening them: the translucent *seed* is a
genuinely reachable authored rejection — an authored palette can supply
one — rejected at the cell that resolved it and naming that cell, while
the translucent *output* is a checker fixture like the contrast one,
since a lightness-only walk from an opaque seed cannot produce it
without an evaluator defect. Both are proven **per cell** rather than
at registration, per §3.5.2.1, and the output check is proven under an
override-triggered re-execution as well as an eager one, since a check
on only the eager path is a check the override path does not have.
§3.5.2's substitution contract is proven by an override that moves a
cell's pair from `secondary` to `primary` and re-executing the ring
against the surface **actually painted**, with a fixture in which the
un-re-executed ring would have missed 3:1 — the whole point of the pair
being the sole substitutable slot — and by rejection of a registry that
declares the seed slot substitutable. Endpoint reachability is proven
directly: a seed whose
lightness is not a multiple of 1/1000 (0.4237) still evaluates the
terminal candidate at exactly 1.0, so the discrete search covers the
endpoint the totality proof depends on — and that fixture asserts
**both** provenance numbers, `step_index` 577 *and* `delta_l` 0.5763,
because a `step_index` assertion alone would pass an implementation
that recorded the terminal walk as 0.577; the zero-step cells likewise
assert `delta_l` of exactly 0, not merely `step_index` 0; §3.5.2.1
`ring-walk-distance` — the recorded distance is the real gate. Its
threshold behaviour is proven **synthetically**, on surfaces constructed
to place the walk at exactly 300 steps (silent) and at 301 (warns), so
the test binds the gate rather than the palette; the tie-break is proven
the same way, on a seed placed symmetrically between two qualifying
bands so that both directions first pass at the same step index and the
lighter candidate must win; the revision-1
counts are then asserted separately as a *palette* regression — the
three cells above the threshold each warn and name both their
`step_index` and the seed they walked from, the twenty-one routine
walks at or below it stay silent (the threshold is strict, so 300 is
itself silent), and the seventy-two zero-distance cells record a
`step_index` of zero rather than no distance at all. A *warning*, never
a rejection, so a compile that emits it still succeeds. Plus totality asserted directly — for every cell of every
context, **at least one direction reaches 3:1**;
`focus-visible-covered` — every
reachable focus-visible cell of a family carries an indicator (lone
token or derivation), with `disabled` exempt because a disabled control
takes no focus and the shipped button suppresses its indicator there,
so requiring one would gate a state the user cannot reach; §4.2
forbidden-construct rejection (component
names / selectors / expressions / undeclared axes in data); §4.3
ambiguity rejection; §4.4 mandatory-base rejection; §4.6 coverage
diagnostics; §5.3 typography-state rejection; §6.4 removal-aware
normaliser test (marker removal observed, not just `Changed<>`); §7's
per-row invalidation tests; §8.1's audit + removal tests; §9.3/§9.4
domain-constrained override validation — every pair classified for
every substitutable recipe; exclusions surviving in the artifact with
typed reasons and refused by the model-side query; every admitted
substitution evaluated; an admitted evaluation or postcondition failure
fatal; and the compiler's product walk and the resolver's availability
query proven to consult the same stored classification rather than two
derivations of one rule; §9.6 type-preservation rejection; §10.3
contrast-position rejection; §11.6 embedded-default unit test; §12.1
wake-publication test (every content-mutation path publishes
design-source-changed); §12.3 selection-lane test (a contrast-only
change triggers the pass; persistence + re-read round-trips all three
axes; a legacy two-field selection event defaults to
`contrast: normal`; a legacy two-axis *persisted record* loads as
`contrast: normal` through both startup and focus-gained re-read);
§13.4 drift test.

**Review obligation (mechanically untestable):** §14.3's
must-not-preclude-`designd` constraint is checked at design review of
any compiler data-model change, not by a test.

**Deferred by decision** (data model must not preclude; adoption needs
its own decision): `SurfaceLevel`-style ambient context axis (one enum
component + one table axis, never a cascade); app-owned family-schema
extension; `designd` ABP surface; animation/transition tokens.

**Migration** proceeds in the proposal's six slices (button-first per
decision 9): 1 — compiler + button family + audit (metrics enter the
live lane); 2 — state normaliser + toggle/checkbox; 3 — remaining
chrome + registry + typography transition + vocabulary alias-window
close; 4 — wake + web emitter + feathers adapter + drift test; 5 — app
family adoption; later — the deferrals above. Each slice ships value
alone; none is authorised by this chapter itself.
