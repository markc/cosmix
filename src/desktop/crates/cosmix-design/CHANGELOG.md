# Changelog

## 0.17.1

- The embedded revision-1 default failed to compile through the public
  `compile_design` entry point: every light modifier block introduced
  `palette.foreground.quiet`, a path the base document never declared,
  and the modifier-path law (`modifier-introduces-unknown-path`) is
  fatal before any flattening. The anchor is now authored in the base
  (the ocean light value) and overridden by the five non-ocean light
  blocks; the ocean light block drops its now-identical override.
  Consumers observe the repair: ced's embedded-design compile — and with
  it the compiled palette — is restored (ced 0.1.4).
- An alias-filled `elevated`/`popover` copy is now diagnosed on its
  donor's authored path, once: a popover-only design collapsing onto
  `base` is refused with exactly one `surface-not-distinct-from-base`
  diagnostic naming `pairs.popover`, and the `elevated-text-fallback`
  warning likewise fires once for the authored donor instead of twice.
  The copy's derived foreground is named for its donor
  (`derive:popover.foreground`).
- The elevated-text fallback's provenance no longer lists the derived
  foreground as a dictionary primitive: the authored-pair token path
  names the surface (and backdrop) primitives only, so every recorded
  token path resolves.

## 0.17.0

- The text half of the `elevated` pair — and of `popover`, its alias — is
  now a compiler derivation: the authored foreground is the preferred
  candidate, and when it misses WCAG AA on the pair's rendered surface the
  compiler delivers the guaranteed knockout (the opaque black or white
  extreme the surface contrasts more with) with an `elevated-text-fallback`
  warning. Every other authored pair keeps the fatal text-contrast gate.
- `popover` joins `muted` and `elevated` in the `surface-not-distinct-from-
  base` gate: an explicitly authored popover equal to `base` is now refused
  even when `elevated` itself is distinct.
- Light-mode `muted_text` now uses the new quiet foreground, not the
  default foreground: a light-only `palette.foreground.quiet` anchor
  (L 0.45, scheme chroma/hue) joins every light modifier block and the
  `muted` pair's foreground rides it there, clearing AA on the muted,
  base and elevated surfaces in all six light schemes. Dark contexts keep
  the default foreground on the muted pair, as before. Consumers observe
  the change: ced's infobar message text and enabled secondary-button
  labels return to primary text (`t.text`) in light mode, since their
  card surface renders as the base page colour (ced 0.1.3).

## 0.16.0

- Add the `elevated` text pair (popover/tooltip/menu role) to the closed
  vocabulary; `popover` becomes its compatibility alias and a source
  authoring only one of the two compiles with the other taking the same
  authored or derived form. The embedded revision-1 design authors
  `elevated` only, on a new desktop-only `palette.background.elevated`
  anchor, and its compiled `popover` delivers the elevated surface.
- Give the embedded `muted` pair a real quiet surface via a
  `palette.background.muted` anchor (lighter than base in dark, darker in
  light) with the default foreground; the transparent-over-page pair it
  replaced moves to `card` unchanged, and the Ghost button variant and
  its focus ring follow, so ghost buttons deliver identical bytes.
- New `surface-not-distinct-from-base` compile error: the `muted` and
  `elevated` rendered surfaces must each differ from `base` by at least
  1.25:1 luminance contrast, with the diagnostic naming the role. The
  embedded palette clears the floor in all twelve scheme/mode contexts.
