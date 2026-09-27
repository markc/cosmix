# Changelog

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
