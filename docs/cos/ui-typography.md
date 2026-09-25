# Application chrome typography

Desktop defaults come from `cosmix-design`'s
`design.v1.typography.records`. The embedded defaults are:

| Role | Preferred family | Weight | Logical px |
| --- | --- | --- | --- |
| `ui` (panel, menu, toolbar) | SF Pro Text | Light 300 | 44/3 (11pt) |
| `ui_display` (window title) | SF Pro Display | Light 300 | 44/3 (11pt) |
| `small` (secondary copy) | SF Pro Text | Regular 400 | 32/3 (8pt) |
| `mono` (fixed width) | SF Mono | Light 300 | 16 (12pt) |
| `terminal` | SF Mono | Light 300 | 64/3 (16pt) |

Each record contains `family`, ordered `fallbacks`, `generic` (`sans_serif`
or `monospace`), `weight` (1–1000), and exactly one of `logical_px` (finite,
positive) or the existing `type_step` metric reference. Optional `line_height`
remains supported. For example:

```mix
ui: {
  family: "SF Pro Text",
  fallbacks: ["Inter", "Noto Sans", "DejaVu Sans"],
  generic: "sans_serif",
  logical_px: 14.666666666666666,
  weight: 300
}
```

UI roles try Inter, Noto Sans and DejaVu Sans in that order before the system
sans family. Mono roles specify DejaVu Sans Mono, Noto Sans Mono, Noto Sans Mono CJK SC (Han, Hangul and Kana; Regular only), then system
monospace. Consumers request Light (300) only when the resolved family has a
face in 300–399 (or a variable weight range containing 300); otherwise they
request Regular (400). This keeps DejaVu Sans on Regular even when ExtraLight
(200) is installed. Apple fonts are
proprietary and are referenced only by family name; install them separately.

`default_typography(TypographyRole)` reads these exact records once without
compiling widget tables. A compiled design exposes `typography().role(role)`;
`active_typography` prefers that record and uses embedded defaults for missing
roles. CTK reads Small/Mono from its live compiled design, including reloads.
The compositor compiles the shared design at startup for `ui_display` and its
fallback chain; a missing or invalid design uses embedded defaults. Chrome
design changes require a compositor restart.
The legacy v0 crosswalk and `button.md`/`button.sm` metric records are retained
for compatibility; they are not the desktop's default UI font authority.

The compositor maps `ui_display` into its title metrics for every chrome
style. Explicit free families precede last-known-good and system UI rescue;
embedded DejaVu remains the final rescue when discovery is unavailable.
Free-chain resolution has its own diagnostic rung and never replaces the
last-known-good theme family.
Its glyph chain preserves the explicit family order before platform families.

CTK's `CtkTextRole::Ui` and `CtkTextRole::Small` apply exact logical sizes and
weights. Panel, menu and toolbar labels use UI; secondary labels use Small.
Button reconciliation applies UI even for compact button geometry. Untagged
legacy widget text keeps its authored weight and still scales from its authored
13px baseline; semantic
roles bypass that multiplier entirely. Output scale is applied once by Bevy.
`CtkTextRole::Mono` applies the fixed-width token at 16px; Quoin's clock uses
it. Existing `CtkMonospace` labels gain the mono family chain and weight while retaining
legacy authored sizing unless an exact role is supplied.
Small uses the UI family chain by default, retaining its independent size and
weight when the UI family or size is overridden. A design's Small family or
fallback override selects its own first available family. Only that one family
reaches shaping (Bevy's `FontSource` carries a single family), so glyphs it lacks
come from the platform fallback rather than the design's later Small fallbacks;
the default Small, which shares the UI chain, is unaffected. Terminal consumers apply
the terminal record separately; terminal rendering is outside this change.

Set `COSMIX_UI_FONT` to a system font family name, for example `SF Pro Text`,
and `COSMIX_UI_FONT_PX` to a body size in logical pixels. These deployment
overrides apply to every CTK application and take precedence over shared and
application theme typography, including theme reloads. Restart apps after
changing their environment.

Whitespace-only family names are ignored. Sizes must parse as finite numbers
between 6 and 96 inclusive; invalid values leave the default/theme value in
effect. Missing families try the explicit free chain, then system sans;
last-known-good and Bevy's embedded face remain available if none resolve.
CTK retains the complete resolved chain for glyph fallback and reasserts it
after collection changes.

Theme files can override `typography.family`, `body_px`, `weight` and
`fallbacks`. Overriding only the family defaults its weight to Regular (400),
including `COSMIX_UI_FONT`; an explicit theme weight is honoured. The embedded
SF defaults retain Light (300). Family and size environment overrides retain precedence on
reload. Quoin optionally imports Plasma's `[General] font` from `kdeglobals`
when `COSMIX_IMPORT_PLASMA_FONT=1`. Import includes Qt 5/6 weight conversion
and point-to-logical-pixel conversion; without opt-in, KDE configuration does
not affect the defaults.

Font acceptance tests shape actual compositor and Quoin panel/menu/small
text, inspect each selected face's family, OS/2 weight and collection index,
and rasterise the same glyphs to `target/font-probes/*.png` at 1× and 2.5×.
SF tests print a loud `SKIP` and run no assertions unless SF Pro Text, SF Pro
Display and SF Mono are all discoverable; a directory alone is insufficient.
Isolated fallback tests disable system discovery and load only free fixtures,
proving DejaVu Sans Regular wins over an unrelated system generic. When present,
the host's `/usr/share/fonts/TTF/DejaVuSans-ExtraLight.ttf` is also loaded to
exercise the ExtraLight regression; it is never vendored. If absent, only that
fixture assertion is skipped. The tests need no GPU.

CosMix Term uses the dark variant of its selected chrome palette. Its menu
bar uses `ctk.panel` with white titles; dropdowns use `ctk.master.panel` and
`ctk.text`. CTK's optional `CtkThemeMode` resource keeps an application's chosen
light/dark mode across reloads without replacing its typography. Split panes
have no separator gap. Only the active pane has a 1px accent border.
