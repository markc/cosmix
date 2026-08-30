# Changelog

## 0.50.0 — 2026-08-30

- Load a complete `design` section directly from the existing shared or
  per-app `theme.conf.mix`, with `app ← shared ← embedded` source precedence.
  Selection-only files contribute no design source; partial or invalid design
  documents retain the last-known-good compiled table.
- Watch the parent directories of both theme files through `notify`, coalesce
  save bursts, and wake reactive Winit apps so changed button colours and
  metrics repaint live. Focus gain and Bus `theme.changed` use the same reload
  path as missed-event backstops, and byte-identical saves are no-ops.
- Resolve the shared file through `cosmix_config::store::config_dir()`
  (`COSMIX_ETC`, then a located checkout's `$COSMIX/etc`, then XDG/FHS), read
  each layer once with a 4 MiB cap, and detect design authority from strict-map
  key presence. Invalid or mid-save palette layers retain their last-good
  values and design authority as one last-good layer and are logged once;
  invalid design sections keep the last-good compiled table.
- Restrict target-file watcher triggers to committed write-close, rename-to,
  removal and rescan events so CTK neither self-triggers nor reads a transaction
  before commit. Parent-directory events, focus and Bus reloads verify directory
  identity and reinstall a watch after replacement. Relative configured paths
  are made lexically absolute, while real paths are re-resolved on every
  reload so replacing a directory or file symlink moves the active watch.

## 0.49.0 — 2026-08-30

- Make the compiled `cosmix-design` button table the sole authority for every
  `CtkButton` colour, focus ring and layout metric. The embedded revision-1
  design compiles at plugin initialisation and re-syncs for scheme/mode
  changes. Replacing the complete source in memory through
  `CtkDesignStatus::replace_source` recompiles once and restyles existing
  buttons; invalid source retains the last-known-good design. On-disk
  `theme.conf.mix` `design` sections are not read yet.
- Deprecate the public `CtkThemeMetrics::button_*` fields. They remain for
  source compatibility but are inert for `CtkButton`; the design table now
  owns height, minimum width, horizontal padding, border width and radius.
- Enable `cosmix-design/compiler` in CTK's runtime dependency. This adds
  `cosmix-lib-mix` and its Tokio, signal-hook and rand graph to every CTK
  consumer, including `default-features = false`; that deliberate cost is
  accepted because the shell is the Mix-UI target and will carry the
  interpreter regardless.
- Fix the icons-feature catalogue parity test to resolve the shared
  `cosmix-interactd` catalogue at its monorepo path.

## 0.47.0 — 2026-07-30

- Add the default-off `body-view` reading surface behind a mandatory type-state
  boundary: raw `BodySource::Html` must become an unforgeable
  `SanitizedHtml`/`SanitizedBody` before projection or layout can see it.
  Ammonia 4.1.4 is the sanitizer because its maintained html5ever allow-list
  handles malformed browser syntax and obfuscation without importing a browser
  engine; script, style, iframe, object and embed content, form semantics, event
  attributes, active URL schemes and non-image data URLs are discarded.
- Make remote content opt-in by construction. Remote image and CSS `url()`
  references are inventoried from one parsed raw DOM and removed, while inert
  `cid:` and raster `data:image/*` references remain available to a later
  rendering arm. DOM traversal covers media/resource attributes, standards-
  tokenised `srcset`/`imagesrcset`, entity-decoded values and cssparser-
  tokenised CSS `url()`/`@import`/`image-set()` resources only inside real style
  attributes/subtrees. String literals containing inert `url(...)` text are no
  longer false positives. The sanitizer allow-list and inventory are one
  synchronised boundary: allowed tag attributes come from one constant, and a
  guard test rejects any new attribute until it is classified as inert,
  navigation-only or inventoried, and proves every classified fetch attribute
  is absent from sanitizer output using a surviving safe control element and a
  representative hostile value. The Stage A feature
  dependency tree has no HTTP-client crate, so the widget performs no automatic
  remote-resource fetching; remote anchors survive only as `LinkActivated`
  payloads. Inventory also covers fetch attempts on controls and resources
  removed wholesale by sanitisation: image-submit `input.src`, form
  `action`/`formaction`, `video.poster`, `object.data`, hyperlink audit pings
  and the obsolete app-cache manifest.
- Bound CSS resource inventory recursion to the same 64-level discipline as
  projection, because a small forest of nested functions could otherwise
  overflow the process stack well below the ingress cap. `RemoteRefs` now says
  when capped input, guarded CSS or an unclassifiable resource URL made its
  inventory incomplete, and quoted `url()` targets are committed only after
  the whole function parses, avoiding false references from invalid trailing
  tokens. Fetch references are classified by the pinned WHATWG `url` parser
  rather than string prefixes, covering canonicalisable backslash, single-
  slash, scheme-relative, mixed-case, whitespace and control-character forms;
  ambiguous values are over-reported and make the inventory incomplete.
- Make `RemoteRefs::is_complete()` fail honest for opaque nested documents,
  meta refresh and foreign SVG/MathML markup. Direct SVG `href` fetch attempts,
  including image, use and filter-image references, are still inventoried;
  sanitisation continues to remove the constructs and never fetches them.
- Project the safe tree into ordinary scrollable Bevy text with headings,
  paragraphs, nested list markers and quote bars, table rows, bold/italic,
  monospace code/preformatted blocks and underlined links. Link activation is
  an entity event carrying the safe href; CTK never opens it. Pointer, keyboard
  and AccessKit `Click` requests share that event path. AccessKit exposes
  document, paragraph/heading/list/list-item/blockquote, text-run/code and link
  roles, including zero-based set positions expected by AccessKit.
- Keep list accessibility ownership explicit across flat text projection:
  continuation paragraphs and later nested lists remain children of their
  owning list item, while quoted items expose `List` → `ListItem` directly with
  `Blockquote` below the item. Link pointer activation now also leaves keyboard
  focus on the activated link instead of letting the containing block steal it,
  and only primary pointer clicks activate links; secondary and middle clicks
  remain available for application policy. Consumers are explicitly required
  to apply their own policy to emitted URLs because CTK deliberately never
  opens them.
- Preserve nested-list semantics through transparent wrappers and emit direct
  nested lists in document order, including text which follows a nested list.
  Table cells now delegate structural flow to the same block/inline traversal as
  top-level content: headings, paragraphs, lists, nested tables, blockquotes and
  block-containing anchors retain their roles and boundaries, while inline-only
  rows keep their compact two-space cell projection. This removes the parallel
  table-cell traversal which had repeatedly drifted and flattened newsletter
  cards.
- Finish that traversal unification for list items as well: top-level flow,
  structural table cells and list-item contents now share the sole child
  block/inline dispatcher. List ownership is separate projection metadata, so a
  heading, paragraph, quote, nested list or table inside `li` keeps the same
  block kind, typography and AccessKit role it has at top level instead of the
  first block being retyped as a 13px list item.
- Preserve signed ordered-list starts, including negative numbering, while
  retaining the existing safe default for malformed or out-of-range values.
  Selection-run ranges now include any list-marker byte prefix before a rule;
  a spawn-level invariant proves every run is ordered, non-overlapping and
  collectively identical to its block's logical copy text.
- Preserve ordinary safe block wrappers rather than unwrapping away their text
  boundaries. `center`, definition lists and the broader HTML block-wrapper
  set survive as inert structural containers; `form` attributes and semantics
  are stripped by converting its sanitised boundary to `div`, while its
  headings, paragraphs and labels remain readable. The post-sanitizer form
  rewrite now has a release-mode fail-closed invariant, so permitting a form
  attribute later cannot silently bypass the bare-tag conversion. Script,
  style, iframe, object and embed contents remain discarded.
- Make every projected accessible node physically descend through accessible
  intermediates to the document. List markers and first-row block content now
  sit under an explicit accessible row below `ListItem`; continuation
  paragraphs, quotes, nested lists, table-cell blocks, headings and links retain
  an unbroken `Document` ancestry instead of being hoisted to the window root.
  The ancestry audit now inventories every AccessKit node CTK intended to
  create before walking parent chains; a detached node or a missing accessible
  list-content row therefore fails instead of disappearing from the candidate
  set.
- Add press-drag-release and Shift-click selection across the complete projected
  copy stream. CTK uses Bevy 0.19's public `ComputedTextBlock::buffer()` Parley
  layouts for byte hit-testing and `TextLayoutInfo::selection_rects` for native
  UI highlight paint, reapplying geometry after every text-layout pass because
  relayout clears it. Anchor/focus live in block-index + UTF-8-byte space and
  copy normalises reversed ranges; HTML separators and rendered list markers
  map into the same exact string copied by Ctrl/Command+C. Empty selection keeps
  the existing focused-block/document copy policy. Both focused and unfocused
  highlight colours use `ctk.row.selected`, avoiding mixed colours across
  separately focused text entities. Logical copy is complete while hostile
  projections are limited to 4,096 painted or fallback-hit-tested runs per
  frame. Copy reaches the OS clipboard only when the application enables CTK's
  default-off `system-clipboard` feature; otherwise it remains in Bevy's
  in-process buffer.
- Admit selection presses only from projected text runs, their block padding,
  links or the document copy target, never from another control which merely
  shares the body-view root; dragging either scrollbar therefore leaves focus
  and selection untouched. A link click following a drag which made the
  logical selection non-empty is consumed once, despite Bevy 0.19 emitting
  `Click` before `Release`; same-cursor pointer jitter remains an ordinary
  click, and Enter/Space activation is unchanged.
- Track selection gestures per pointer in a fixed 16-entry table. The first
  active primary pointer exclusively owns the shared anchor/focus; simultaneous
  pointers are retained but cannot alter the selection or activate links.
  Consuming Click suppression no longer tears down gesture state needed to own
  the following Release and DragEnd events. The bound is a tracking bound, not
  an activation boundary: a pointer the full table turned away can never reach
  a selection-mutating path, so its click stays an ordinary click, and link
  activation never depends on whether some unrelated pointer released first.
- Latch shift-click intent when the press happens instead of re-reading the
  keyboard when the click arrives. `Pointer<Click>` carries no modifier
  snapshot, so a gesture whose Shift state changed between press and release
  used to both extend the selection and activate the link — or swallow an
  activation it should have delivered. A Shift press is admitted before
  hit-testing, so the latch survives a press which resolves no text position
  under the pointer; a plain press is still admitted afterwards and therefore
  starts no drag when it lands on nothing.
- Keep that pointer-gesture table independent from logical selection clearing,
  so Escape cannot discard armed link-click suppression or ownership of the
  pending Release and DragEnd while a drag remains live.
- Treat pointer cancellation as immediate terminal selection-gesture teardown,
  then backstop cancellations delivered while hovering elsewhere with a `Last`
  system which prunes records whose Primary button is no longer pressed. Bevy
  applies every trigger queued by its `PreUpdate` pointer-events run before
  `Last`, so any number of batched Press observers leave earlier records intact
  for their pending Click, Release and DragEnd observers; dead records are gone
  before the next frame can exhaust the 16-pointer bound.
- Bound raw HTML cheaply before parsing at 2 MiB; plain text is first
  canonicalised from CRLF or lone CR to LF and then capped at 2 MiB. Cap an
  individual projected text run at 64 KiB and split longer accepted content
  into further bounded runs without truncating the body. Chunk boundaries keep
  extended grapheme clusters whole, prefer a nearby CSS-collapsible word
  separator and avoid placing entity boundaries against no-break spaces, so
  separately shaped text entities do not split combining marks, joined emoji,
  ordinary words or an author's no-break opportunity. A single pathological
  grapheme larger than the nominal run cap remains indivisible. Content
  budgets remain a second defence against pathological expansion, but their
  defaults rise to mail-sized 4,096 blocks and 32,768 styled spans and callers
  can tune both through `CtkBodyViewProps`. The alert is explicitly outside
  those content budgets, so hitting the span ceiling no longer discards the
  in-progress block. Deep-list and every recursive projection entry share a
  depth guard and flatten retained text at the limit, preventing hostile nesting
  from bypassing the stack bound. Focus paint is change-gated instead of
  scanning every block on idle frames. Body-view virtualization through
  `virtual-list` is deliberately deferred beyond v1.
- Preserve standalone quote-only lines (`>`, `>>`, `> >` and nested variants)
  as plain-text body content across LF, CRLF and lone-CR input. Only genuinely
  blank lines separate paragraphs: imperfect reply-chain paragraph layout is
  preferable to deleting a line from a code sample, transcript or quoted body.
  Canonicalisation and DOM text flattening stay whole-body bounded, while
  projection and image-alt rendering emit bounded run-sized allocations.
- Preserve plain-text block edges and whitespace-only lines byte-for-byte
  instead of applying HTML's collapsible-whitespace trimming to ordinary mail.
  This keeps indentation, Markdown hard breaks, tab-only lines and aligned
  ASCII tables intact. Leading, trailing and repeated zero-length lines now
  remain in projected spans too, so concatenating plain projected span text
  exactly reconstructs the canonical LF input; HTML projection retains its
  deliberate edge trimming. HTML collapsing and trimming now recognise only
  CSS's space, tab, line feed, carriage return and form feed; NBSP, narrow NBSP,
  ideographic space and every other Unicode space remain exact printable
  content, including at block edges and run-chunk boundaries.
- Make whole-document plain copy use that exact canonical span stream, without
  injecting another LF between blocks which already retain their separators.
  HTML copy is deliberately a readable text projection: blocks are joined by
  one LF, rendered list markers and rules are retained, and the synthetic
  truncation alert is omitted.
- Intern each distinct navigation target once and keep only a small,
  projection-owned handle in styled spans. Resolving a handle against another
  projection fails closed instead of accepting the same numeric index. Anchors
  above the public 8 KiB URL cap deliberately degrade to plain text and are
  dropped rather than copied tens of thousands of times.
  A separate 40,000-entity ceiling now charges the fixed scroll shell, content,
  text runs, list containers and markers, quote wrappers and truncation alert.
  Exact live-entity tests cover every projected block kind, headings,
  preformatted and table paths, quote/list/marker shapes, truncation and the
  empty fallback, so conditional spawn or ledger drift fails the suite; the
  empty fallback also respects zero block/span budgets.
- Keep URL interning for bounded storage while assigning every source anchor a
  separate occurrence identity. Styled or chunked anchors now expose one
  complete AccessKit link and one tab stop across the whole projection, all
  visual runs activate that same target, and adjacent anchors sharing an href
  remain separate links. Flow-content anchors used for newsletter cards retain
  their heading, paragraph and list blocks, propagate one occurrence through
  every block, and build one accessible name with explicit block separation.
- Add the per-instance `RenderArm::Text | RenderArm::Engine` seam now; Engine
  is deliberately unimplemented in Stage A and resolves gracefully to the
  permanent text fallback. The demo switches plain, simple HTML, newsletter
  and hostile bodies, reports suppressed remote references, logs link events
  and exposes the renderer toggle stub.

## 0.46.0 — 2026-07-30

- Add an unconditional wrapped `CtkTextArea` for compose-sized plain text.
  The source spike found Bevy 0.19's Parley-backed `EditableText` to be a
  sound multiline engine — newlines, soft wrapping, bidi, visual-line motion
  with retained goal column, selection, cursor-following vertical scroll,
  clipboard and IME preedit/commit are already one coherent path — so CTK
  rides it rather than creating a second editor core.
- Supply the layer Bevy deliberately leaves open: transaction-level bounded
  undo/redo with exact selection restoration and deliberate adjacent-typing
  coalescing, viewport-derived Page Up/Down, wheel/trackpad scrolling,
  atomic selected-IME transactions, selection-aware max-length and
  read-only/IME policy, real `EditableTextFilter` enforcement, focus-border
  reuse, and observer-style change, window-scoped deactivation blur and
  Ctrl+Enter submit events. Programmatic replacement is one bounded history
  transaction and wheel position survives idle cursor-follow frames. Correct
  Bevy 0.19's source-level third-click mismatch by selecting the hard line
  rather than the whole buffer.
- Layer a multiline AccessKit input and separate per-hard-line text runs over
  the editor, remapping canonical anchor/focus into the containing run. Runs
  are intentionally unlinked across hard-line boundaries because AccessKit's
  line links mean the same rendered line. Bevy does not yet expose editable
  per-character geometry or handle AccessKit selection actions, so soft-wrap
  line links, geometry and selection commands remain documented limitations.
- Keep native system clipboard access default-off behind `system-clipboard`;
  the dependency-free core degrades to Bevy's in-process clipboard. Add an
  opt-in compose demo with ordinary single-line To/Subject fields, tab order,
  wrapping, keyboard-accessible undo/redo controls and a live body character
  count.
- IME preedit rendering, selection, value exclusion, commit delivery and
  Wayland candidate positioning were source-verified. Synthetic `Ime` tests
  exercise selected preedit cancellation, accepted and oversized selected
  commits at maximum length, filter enforcement, and cancellation on a
  read-only flip. This machine has no fcitx5/IBus and the automated session has
  no interactive seat, so live Wayland CJK testing remains outstanding.

## 0.45.0 — 2026-07-30

- Add the default-off, dependency-free `virtual-list` widget for data sets
  whose row count must not become an entity count. A fixed-height spacer keeps
  Bevy's native pixel scrolling and scrollbar geometry honest while CTK
  recycles only the viewport plus configurable overscan; applications retain
  ownership of row content through a stable-ID model and bind callback.
- Keep selection and reset scroll anchors in `RowId` space so inserts, removals
  and reordered refreshes do not silently move the user's context. Mutation
  hints rebind only affected realised rows where possible, and every realised
  AccessKit list item reports its position within the full data set.
- Add a 100,000-row mail-like demo with jump, selection-mode and 100-row/s
  mutation controls. The widget records its complete per-frame CTK contribution
  in CTK's fixed-bucket latency histogram and the demo prints that summary.

## 0.44.1 — 2026-07-28

- After successful shared-theme persistence, broadcast an unretained local
  `theme.changed` invalidation/wake and make broker-verified local deliveries
  re-read the durable shared file through a dedicated latest-valid-wins inbox.
- Wake Bevy's Winit event loop when an idle app receives a theme change, while
  retaining focus-gained shared-file reload as the missed-wake backstop.

## 0.44.0 — 2026-07-27

- Make the knockout reachable, and reach it. `ctk.row.selected` is no longer
  the midpoint between panel and accent; it is walked away from `ctk.panel`
  until the two are separated by 7:1, keeping the accent's hue and chroma. The
  selected row is now a solid accent bar, and `ctk.row.selected.text` falls out
  of the *existing* derivation as exactly `ctk.panel` — the knocked-out look
  0.43.0 recorded as structurally unreachable, now reached with no special
  case. Asserted per palette, and the 0.43.0 test pinning it as unreachable is
  deleted, because that statement is now false.
- The 7:1 target is measured, not preferred. AA's 4.5:1 is enough for the
  knockout alone, but at 4.5 three of the twelve scheme/mode palettes leave
  *zero* lightness headroom for a dimmed variant, which would collapse it onto
  the main foreground. At 7:1 every palette keeps at least 13 lightness units.
- Add `ctk.row.selected.text.dim` for selected-row metadata, derived by dimming
  the knockout back toward the bar as far as AA permits. Asserted to clear AA
  and to remain distinct from `ctk.row.selected.text` on every palette.
- Repair `ctk.text.dim`, which 0.43.0 could only pin as *failing* AA on every
  row and in every dark-mode resting pane. It is now derived to clear AA
  against both surfaces it is actually painted on, the panel and
  `ctk.row.hover`, and is asserted against both. The pinned-failure floors are
  replaced by real guarantees.
- Hover and press on a selected row now emphasise *away* from the knockout
  rather than in a fixed direction, so contrast rises with the interaction
  instead of collapsing into it. The direction is derived from the palette, not
  from a `Mode` flag, because a user theme file can falsify the flag. The full
  authored lifts survive the contrast clamp on every built-in palette.
- **Breaking for struct-literal construction:** `ThemeColors` gains
  `row_selected_text_dim`. Field access and `..Default::default()` are
  unaffected.

## 0.43.0 — 2026-07-27

- Add `ctk.row.selected.text`, a foreground for selected rows whose contrast
  against `ctk.row.selected` is derived rather than chosen. It starts at the
  panel — the knocked-out look a selection wash wants — and walks away from the
  wash, desaturating as it goes, stopping at the first candidate that clears
  WCAG AA. Contrast is measured after gamut clipping, so the answer is what
  lands on the pixel. Asserted at 4.50–4.65:1 for all six schemes in both
  modes, and asserted minimal: no earlier candidate on the walk had already
  cleared. Consumers previously had no token that clears AA everywhere —
  `ctk.text` reaches only 4.31 on mono/light, `ctk.text.dim` falls to 1.07, and
  `ctk.panel`, the obvious choice for a knockout, never exceeds 3.81.
- Record that the knockout itself is currently unreachable: panel-on-wash
  measures 1.82 (crimson/dark) to 3.81 (mono/dark), because `selected_l` is a
  midpoint by construction while the panel sits at an extreme. The derived
  foreground therefore crosses the wash and lands on the conventional side. A
  test asserts this and fails if a future palette makes the knockout reachable.
- Add `contrast_ratio`, `is_opaque` and `AA_CONTRAST` to the public surface, so
  an app deriving a foreground of its own or an agent writing a theme file can
  check the same number this module checks. `contrast_ratio` is defined only for
  opaque colours — it composites nothing, so `#ffffff00` on black measures a
  perfect 21:1 and paints nothing. A theme override that makes either half of
  the selection pairing translucent is reported as unmeasurable rather than
  passed.
- Re-derive `row_selected_text` when a theme file moves `row_selected` or
  `panel` and states no foreground of its own. This is layer-local: the cascade
  does not track provenance, so a later layer moving either half re-derives over
  an earlier layer's explicit value, and a theme wanting its foreground to
  survive must restate it in the same layer. An explicit value below AA stands
  and is warned about with its measured ratio.
- Pin the contrast gaps this release did not close, each at its measured floor,
  so they cannot worsen unnoticed and so the test deletes its own entry when one
  is genuinely fixed: `ctk.text.dim` clears AA on no scheme in either mode on a
  row — 1.07–2.57 selected, 2.89–3.79 hovered — and on a resting pane clears it
  in no dark mode at all and in only four of the six light ones (3.94–5.42,
  missing on stone and mono); `ctk.text` on a selected row misses on mono
  (4.31–4.47); and
  `ctk.control.active` on a hovered row falls to 2.63. Closing any of them moves
  a palette colour every app renders.

## 0.42.0 — 2026-07-27

- Add theme-aware text-input focus borders. Each field retains its own resting
  border token and switches colour to `ctk.control.active` while its editable
  target owns `InputFocus`; plain and secret fields update across live theme
  changes without changing border width or fighting Feathers theme paint. A
  focus border requires `BorderColor`, so a field cannot be spawned into a state
  where the painter silently skips it; a field whose focus target has been
  despawned returns to resting rather than staying lit until the next input
  event clears Bevy's stale focus; and every edge is repainted, not only the
  top, so an external writer cannot leave three edges stale. The painter runs
  after the whole of `Update`, so a live theme change reaches the border in the
  frame it is applied and a field spawned and focused through deferred commands
  is lit immediately rather than rendering one transparent frame. It also runs
  after modal focus sanitation, so a field underneath an opening modal is never
  lit from the focus the modal is about to take.

## 0.41.1 — 2026-07-27

- Add explicit edge or centre placement for toolbar groups while preserving the
  existing edge default. The standard icon path reserves an 80px safe area
  pinned to DCS control geometry, so the outer button on each side which was
  previously painted underneath the higher-z sidebar controls is visible.
  Debug builds warn once when text or application-supplied controls exceed that
  static reservation; every build clips oversized controls to the reserved
  area. Centred groups keep their width and clip symmetrically at the safe
  boundary on narrow windows; labelled toolbar buttons never wrap or paint
  outside their own background.

## 0.41.0 — 2026-07-27

- Add unconditional, grapheme-safe filename middle elision. CTK preserves a
  useful final extension, measures with Bevy's own text pipeline after live
  typography has applied, and re-elides only when the width or effective text
  style changes. The complete source remains available for accessibility.
- Share the same pure elision API with labelled drag-export icons instead of
  retaining a feature-gated, character-based implementation in `icons`.
  Export-label measurement exhaustion now returns a proven-fitting `…` rather
  than an error, and the total measurement budget is 32 calls rather than up to
  33.

## 0.40.0 — 2026-07-26

- Adopt the desktop UI font as a theme value. `TypographySpec { family, body_px }`
  joins colours in the shared/per-app cascade, defaulting to Noto Sans at
  13.333px (10pt at 96 logical DPI). A `PostUpdate` pass stamps managed text and
  rescales it as `authored x body_px / 13.0`, so the authored sizes at each spawn
  site stay as written and repeated live changes do not compound.
- Enable Bevy's `system_font_discovery`, which backs Parley with fontconfig, and
  pin the family through the generic sans-serif mapping rather than by name.
  Naming a family directly narrows fallback candidates to the run's script —
  Latin, for a filename — so symbols the face lacks are more likely to land as
  tofu. The generic route keeps fontconfig's full coverage ordering instead.
- Load fontconfig through `dlopen`. Linking it normally puts `libfontconfig.so.1`
  in the binary's `NEEDED` list, and a host without it is killed by the dynamic
  loader before `main` — an outcome no in-process font fallback can survive.
- Reconcile managed text rather than stamping it once. A *size* written by
  anything else is re-adopted as the new authored intent instead of being
  reverted at the next theme change, including one written before any mapping
  existed. A reassigned *font source* is restored, not re-adopted — CTK owns the
  face of everything it manages, and `CtkTypographyOptOut` is the way to keep a
  bespoke one. Text already at its computed value is left alone, so an idle
  frame triggers no text rerender, and a size CTK cannot derive from — a
  non-finite one — is left exactly as its author wrote it, and reconsidered if
  it is later corrected. External writes are recognised by value, so writing
  exactly the size CTK last applied is indistinguishable from CTK's own and does
  not become the new authored intent.
- Bound the configured base size to 6–96px. Theme files outside that range are
  rejected with an explicit error; every other route, including a hand-built
  `TypographySpec` passed to `apply_theme`, is clamped where the size is
  consumed. A mistyped `13333` would otherwise ask Bevy to rasterise glyph
  atlases thousands of pixels tall during startup.
- Add `CtkTypographyOptOut` for text that must not follow the process-wide
  policy, and apply it to the monospace text view. Code editors, icon fonts and
  deliberate display faces must opt out in the same spawn as their `TextFont`.
- Expose `CtkTypography` as live introspection: effective family, requested
  family, provenance per field, and whether the requested family, the last
  known-good one, or the embedded fallback is in effect. An unavailable family
  warns once, keeps the last good mapping, and is retried on later passes rather
  than cached against the theme revision that missed. Ownership of that mapping is
  re-asserted every pass instead of assumed, in both the settled and the fallback
  case, so a collection rebuilt underneath CTK downgrades what this resource
  reports rather than leaving it naming a family nothing is rendered in. Losing a
  mapping does not strand managed text: the configured size keeps applying, and a
  source already stamped is kept rather than unwound to the ASCII-only embedded
  fallback. The configured size still
  applies when the family does not resolve, so the same theme file does not mean
  two different things depending on process history. A theme typo cannot stop an
  app opening.

## 0.39.1 — 2026-07-26

- Add an optional deferred export label: `DragSource::with_export_label` carries
  a filename, and at the export threshold CTK renders a bounded, sanitised pill
  through usvg and composites the shared square raster onto it with tiny-skia.
  Rows keep only the square raster and a string, so the wide raster is built
  once per drag rather than once per directory entry. Font discovery warms once
  per process through `warm_export_label_fonts`. Any label failure logs and
  falls back to the square icon without refusing the drag.
- Add `ExportIconRaster::logical_anchor`, so a raster carries its own pointer
  hotspot instead of the export path assuming the geometric centre. Unlabelled
  rasters still default to their centre; a labelled pill anchors over the icon.

## 0.39.0 — 2026-07-26

- Escalate every exportable primary-mouse file drag when it crosses CTK's
  four-pixel threshold, while the Wayland implicit grab is still attributable,
  instead of waiting for `CursorLeft`, which compositors suppress for the life
  of that grab. The `Armed -> Dragging` transition happens first but ghost
  creation is deferred: a successful compositor handoff never creates a Bevy
  ghost, while any refusal continues as the same intact in-app drag.
- Preserve CTK's payload and Bevy pointer state until `start_outgoing` succeeds.
  Path validation now happens once in the transport payload constructor; an
  invalid path, missing bridge, ambiguous seat, missing grab or other start
  refusal leaves the payload, pointer press, click suppression and ordinary
  ghost lifecycle unchanged.
- Send a `DragSource` export raster through `start_outgoing_with_icon` when the
  bridge advertises icon support, centred on the pointer. Sources without a
  raster and bridges without the required globals keep the iconless start.
  `CursorLeft` is again an ordinary cancellation input for drags which remained
  inside CTK.

## 0.38.0 — 2026-07-26

- Add `DndDrop::decision_requirement` (`DropDecisionRequirement::{None,
  Wayland}`), stating whether an `Ask` delivery needs the Wayland-protocol
  decision step before the application may act. **Breaking**: `DndDrop` gains a
  public field.
- Set it from the delivery path — `None` in-app, `Wayland` for anything
  `deliver_wayland_drop` emits — never from `DndOrigin`. Once a drag escalates
  at the drag threshold, a drop back into our own window leaves via the
  compositor and returns through the nonce echo, so it is Wayland-delivered
  while its origin stays internal. Tagging from the origin would start the file
  operation without answering the protocol, and the source would time out after
  the copy had already run.
- Rename the delivery-path vocabulary from `external` to `wayland`
  (`WaylandTransfer`, `wayland_active`, `deliver_wayland_drop`,
  `apply_wayland_acceptance`, `propose_wayland`, `clear_wayland`, and the
  remaining helpers). `DndOrigin::External` and `IncomingRoute::External` keep
  their names — they classify the source application, which is a different axis
  and stays true.

## 0.37.1 — 2026-07-26

- Add `ExportIconRaster`, a CPU-owned premultiplied RGBA8 drag icon, and
  `DragSource::with_export_icon` to opt a source into one. The Bevy ghost lives
  inside our window and cannot follow the pointer out of it, so a drag the
  compositor owns needs pixels the compositor can draw.
- Add `IconSet::load_with_rasters` and `IconSet::raster`, rasterising catalogue
  SVGs synchronously through `resvg` and caching by `(icon, size, scale)`.
  Rasterising at the drag threshold would put unpredictable latency at the exact
  moment the gesture starts; both successes and failures are cached for the life
  of the `IconSet`, so a broken asset root costs one read and one warning rather
  than one per row.
- Add `outgoing_icon_from_raster` behind `os-dnd`, converting to the transport's
  validated `OutgoingIcon`. `ExportIconRaster` duplicates the transport's signed
  32-bit width, height and SHM-pool bounds — it must stay usable without the
  `os-dnd` feature, so it cannot import them — and a gated test pins the two
  copies to the same last-accepted and first-rejected geometry.

Nothing consumes the icon yet; the export trigger is unchanged.

## 0.37.0 — 2026-07-26

- Escalate an in-app drag into a real Wayland drag (phase 5b), with own-window
  echo correlation by private per-transfer nonce MIME.

## 0.36.0 — 2026-07-26

- Land the `os-dnd` glue module bridging `cosmix-wl-dnd` into the ctk DnD
  contract, so external drags reach app code as ordinary `DndDrop` deliveries.

## 0.35.1 — 2026-07-25

- Fix Escape-cancelling a drag delivering the pending button-up as a real click
  on the source, which selected the row the drag had just declined to move. The
  suppression is now retained until the press actually ends, and
  `dnd_click_is_blocked` reports it so consumers keep one predicate for the
  live-drag and cancelled-drag cases.
- Backstop that retained suppression with the physical mouse button.
  `bevy_picking` guarantees no event follows a `Cancel` for a pointer and emits
  `Cancel` only to hovered entities, so a pointer lost over no target delivers
  neither `Release` nor `Cancel`; without a level-triggered clear the latch
  stranded and DnD refused to arm for the life of the process.

## 0.35.0 — 2026-07-25

- Add `WithdrawInteraction` and `WithdrawFileRequest` for programmatic,
  result-free retirement of queued or visible modal surfaces when their owner
  has already observed terminal state through another channel.

## 0.33.0 — 2026-07-25

- Complete the in-process dialog suite (Phase 2b). `InteractionRequest` gains
  `choice`/`multi_choice`/`slider`/`text_view` constructors with typed specs
  (`ChoiceSpec`, `MultiChoiceSpec`, `SliderSpec`, `TextViewSpec`) and
  `ChoiceItem` (key/label/description/disabled). Choice resolves to a single
  key, multi-choice to an ordered key list; duplicate keys are de-duplicated
  keeping the first occurrence. The text view is scrollable, read-only, with an
  acknowledge action and optional monospace.
- Add non-modal, owner-driven progress. A `ProgressSpec` card lives in a
  separate `ProgressState` map — never in the FIFO `ModalCoordinator`,
  `ModalCapture`, focus trap or scrim — so it can advance while a modal is
  open. Owners drive it with `ProgressUpdate` and finish it with
  `ProgressComplete`/`ProgressCompletion`; a cancellable card is Tab-reachable.
- Back the integer slider with CTK numeric controls: an f32 fader is the input
  mechanism, but the value is always re-canonicalised to the true i32 grid
  (`normalise_slider_range` swaps inverted bounds without ever widening;
  `canonical_slider_value` rounds half-up and clamps). Degenerate and
  f32-collapsing ranges (endpoints ≥ 2^24, or `min == max`) resolve without
  panicking, and the live value label tracks the resolved grid value, not the
  raw fader f32.
- Focus a choice option scrolls it into view (`FocusGained` observer →
  `ScrollIntoView`); Tab + initial focus are wired. Arrow-key list navigation
  is deferred to a future `ListBox` adoption.

## 0.32.0 — 2026-07-25

- Extract the shared `DcsAppShell` (`ctk::dcs_app_shell`): application chrome
  (menu bar + toolbar + DCS sidebars + centre + status) as one composed ctk
  component. Apps inject content into named slots and never build or patch
  shell structure; slot distinctness is debug-asserted. New default-on `menus`
  cargo feature (`actions` implies it). Tower and FileMgr are the first
  consumers; Studio still hand-assembles. Procedure + contract:
  `_doc/2026-07-25-dcs-app-shell.md` (control repo).

## 0.31.0 — 2026-07-25

- Add the dialog foundation. `InteractionRequest` gains a `kind` enum
  (`Message`/`Confirm`/`Prompt`/`SecretPrompt`, `#[non_exhaustive]`) with typed
  `InteractionOutcome::{Resolved(InteractionValue), Cancelled, Dismissed}`; the
  existing `message()`/`confirm()` constructors stay source-compatible.
- Add `ctk::text_field`: `CtkTextField` (styled `EditableText` with focus,
  max-length, and a validation hook), `CtkSecretField` (self-rendered bullet
  masking — Bevy's `EditableText` masking is unimplemented upstream — with copy/
  cut disabled and a11y value redacted), the zeroise-on-drop, non-`Serialize`,
  non-`Clone` `SecretValue`, and the shared `validate_filename` validator.
  Secret capture is in-process only, never over the wire (B-1).
- Add `ctk::dialog_shell`: the shared scrim/panel/action-row/focus-trap/a11y
  pattern extracted from the interaction and file-requester presenters.
- Unify the modal lane. One FIFO `ModalCoordinator` owns a single
  `ModalCapture` token across every interaction kind and the file requester;
  the file requester routes through it as a nested presenter and `FileRequestId`
  survives only inside a compat adapter that mints fresh internal correlations
  (caller-supplied legacy ids can no longer cross-correlate). Focus restores to
  a still-live invoker, and a per-frame `sanitize_modal_focus` pulls focus back
  to the modal default if it escapes the focus-root subtree.
- FileMgr drops its entire bespoke `NameEditInput` machinery for
  `InteractionRequest::prompt` + the shared `validate_filename` policy; rename
  and new-folder behave identically (initial text selected on rename, inline
  error on invalid name, Enter/Escape).
- Add the `dialogs` CTK example demonstrating every in-process v1 kind.

## 0.30.0 — 2026-07-25

- Add an opt-in third AMP bridge connection dedicated to broker observation,
  with independent request/message bounds, generation, drop accounting, a
  1 MiB envelope cap sized for worst-case JSON expansion, and bounded stop
  flushing during shutdown.
- Add `ctk::chrome`: a shared toolbar-row builder (left/right groups, action-
  wired icon/label buttons) and status-bar widget. FileMgr and Studio drop
  their hand-rolled copies; Tower migrates after its current arc lands.
- Add context menus to `ctk::menu`: pointer-positioned popups reusing the
  menu-bar's `MenuItemDef` presentation machinery (icons, enabled state,
  accelerators, action dispatch) with outside-click/Escape dismissal,
  single-open policy, wrapping arrow/Enter keyboard navigation, and the
  documented `CONTEXT_MENU_Z` layer. FileMgr's bespoke RMB menu is deleted.

## 0.29.0 — 2026-07-24

- Add `ctk::key_input` (actions-gated): the shared `EventKeyState` +
  `normalise()` folding Bevy `KeyboardInput` into cosmix-actions' key
  vocabulary in event-delivery order. Studio and FileMgr drop their private
  near-identical copies and consume the shared module.
- `AppIdentity` moves to the new Bevy-free `cosmix-app-identity` leaf crate
  (src workspace); `ctk::identity` re-exports it source-compatibly. Tray
  consumes the leaf directly and its deliberate duplicate is deleted.

## 0.28.0 — 2026-07-24

- **Breaking:** every non-discovery app-port verb now applies the fail-closed
  local caller gate before handler lookup (previously only `action.invoke` and
  `app.controls.set` gated themselves). Named app verbs — `app.quit`,
  `app.transport.*`, `app.theme.set`, constrained-root loads — reject
  anonymous, unstamped, or wire-asserted-remote callers with RC 10. Read-only
  discovery (`app.describe`, `app.controls.list`/`.get`,
  `actions.list`/`.describe`) keeps its open contract. Closes the 2026-07-24
  cold review's BLOCKER.
- Forward the broker-owned `noded.observe.event` stream from the requesting
  control plane into CTK's bounded telemetry queue without admitting arbitrary
  unsolicited control-plane traffic.

## 0.27.2 — 2026-07-24

- Register both CTK AMP bridge planes with one process-stable provenance
  record, including the executable name, CTK bridge build version/git/time,
  process id, and start time. Supervised reconnects retain the same process
  identity, allowing fail-closed same-process mutation checks to distinguish
  a surviving citizen from a reused service name.

## 0.27.1 — 2026-07-24

- Require noded's broker-owned `broker_origin: local` delivery marker for
  `action.invoke` and `app.controls.set`. Missing, duplicated, or mesh origin
  fails closed; CTK 0.27.1 therefore requires noded 0.6.14 for mutation.

## 0.27.0 — 2026-07-24

- Apply the same fail-closed, same-node provenance gate used by
  `action.invoke` to `app.controls.set`; unauthenticated remote mutation is
  rejected before control lookup or dispatch.

## 0.26.0 — 2026-07-24

- Add the reusable `TopologyCanvasPlugin` with separate node and edge layers,
  free panning, clamped zoom, pointer panning, keyboard viewport controls,
  keyboard-focusable node selection, and endpoint-derived edge geometry.

## 0.25.0 — 2026-07-24

- Add `AppIdentity`, including registry-slug validation and derivation of the
  `dev.cosmix.<slug>` freedesktop app id.
- Adopt identity-derived `Window::name` values in the CosMix Desktop apps.

## 0.24.1 — 2026-07-23

- Keep interaction keyboard handling and request ingestion in disjoint system
  sets so applications can order action routing between the two phases without
  creating an invalid overlapping-set schedule.
- Clarify that `ModalCaptureSystems` covers pre-input modal work, not every
  later service-specific request-ingestion system.

## 0.24.0 — 2026-07-23

- Add `resolve_theme_with_selection` and
  `resolve_app_theme_with_selection` for live scheme/mode changes that retain
  the shared and per-app token/metric override cascade.

## 0.23.0 — 2026-07-23

- Add `CtkThemePlugin`, `ApplyTheme`, and `ThemeState` for revisioned live
  colour application. Theme metrics remain relaunch-only because they shape
  widget geometry at spawn time.
- Re-resolve the shared and per-app theme cascade when a window gains focus,
  allowing one Cosmix desktop app's theme selection to appear in another
  without polling or a file watcher.
- Add transactional shared-theme selection writes. The read-modify-write
  changes only `scheme` and `mode`, preserves token, metric, and unknown
  fields, validates the complete candidate, serializes writers through a
  bounded sidecar advisory lock, then atomically replaces it. Applications
  enqueue `ThemeWriteRequest`; a dedicated worker performs the entire file
  transaction and reports `ThemeWriteCompleted`, so lock contention never
  stalls the Bevy main thread. Controlled `AppExit` closes the bounded queue
  and joins the retained worker, draining every accepted write before exit.
- Make `ThemeSpec::overlay` all-or-nothing and add border, row-hover,
  row-selected, scrim, and danger-surface tokens for application chrome.
  `SCRIM` varies by light/dark mode only; derived role values can legitimately
  coincide where their source palette roles coincide, including
  `DANGER_SURFACE` across the current light schemes.
- Retint existing SVG icons after a live theme change and tokenise CTK's DCS
  shell, requester, interaction, piano-roll, and tree-view colour surfaces.
- Make `apply_theme`/`ApplyTheme` CTK's only provided public runtime mutation
  path. Bevy callers can still mutate `ResMut<UiTheme>` directly, but doing so
  bypasses `ThemeState` and revision-driven consumers.
- `spawn_icon` now takes `&UiTheme` to resolve its initial colour before the
  first frame. Icon consumers must install `CtkThemePlugin` for later token
  re-tinting.

## 0.22.0 — 2026-07-23

- Add the optional `action.invoke`, `actions.list`, and `actions.describe` AMP
  app-port surface for `amp,actions` consumers, publishing accepted calls onto
  the ordinary `ActionRequest` bus with `Source::Amp`.
- Enforce fail-closed caller authority: canonical local noded identities are
  admitted, anonymous/remote callers and wire-asserted identity claims are
  rejected until authenticated provenance can resolve `ctk.actions`.
- Query and validate only registered action metadata, live enabled predicates,
  and typed argument schemas; interactive actions return a stable RC 10 error
  naming their explicit-target direct verb, or an explicit local-only result,
  instead of opening local UI.
- Authorise discovery as well as invocation, reject AMP invocation while modal
  capture is active or an enabled interactive request was produced earlier in
  the current app-port frame, and enforce each action's explicit AMP source
  policy in the core registry.
- Cache `actions.list` by registry + enabled-state revisions so predicates run
  only after invalidation. Action transport backpressure uses stable SPEC-02
  RC 10 identifiers while legacy `app.*` verbs retain RC 11 busy behaviour.
  Oversized action requests likewise return RC 10 `body_too_large`.
- Generalise `MenuActionRegistry` as `ActionRegistryResource` while retaining
  the old name as a compatibility alias; apps mark enabled-state changes on
  this resource to invalidate query caches.

## 0.21.0 — 2026-07-23

- Add ordered, bounded Update-stage arbitration for focused CTK control keys,
  allowing an app router to discard only widget effects later than an accepted
  modal-opening shortcut while retaining pointer immediacy.
- Give controls explicit input policy: faders and knobs accept held-arrow
  repeats, while momentary and toggle buttons ignore repeated activation.
- Make deferred effects semantic rather than snapshot-based: toggles flip the
  live state at application and pointer edits take precedence over queued
  slider keys. CTK owns button/toggle pointer handling and numeric keyboard
  handling without temporarily removing Bevy widget components.
- Bound deferred keyboard effects to 64 entries, dropping the oldest on
  overflow; visible keyboard effects arrive in the arbitration set at most one
  frame after focused-input dispatch.

## 0.20.0 — 2026-07-23

- Treat menu ids as action ids and add revision-keyed enabled, checked/radio,
  and keymap-derived accelerator presentation without respawning menu bars.
- Add optional themed menu icons behind `icons` and an opt-in `MenuActivated`
  to typed `ActionRequest` bridge behind the new `actions` feature. Dispatch
  is scoped per `ActionBridgeBar`, reads current presentation at activation,
  and requires both presentation and the live action registry to enable a
  nullary menu action.
- Keep two-field `MenuItemDef` literals valid with `icons`; provide a
  const-friendly constructor and attach decoration through `with_icon`.
- Keep plain `spawn_menu_bar` rows free of icon slots; icon columns are created
  only by `spawn_menu_bar_with_icons`.
- Defer enabling Fusion's `icons` feature to Phase 3, when its 14 menu literals
  and menu construction are rebuilt once around the shared action constants.

## 0.19.0 — 2026-07-23

- Route id-less `app.*` AMP commands through the registered app-verb dispatcher.
  They remain fire-and-forget: side effects run, but no correlated response is
  attempted without a request id.
- Replace the public eight-argument `paint_region_lane` call with
  `RegionLanePaintParams`.
