# ADR: Cosmix visual identity — curated own-palette + bundled assets, not host-theme inheritance

- **Date:** 2026-07-22
- **Status:** ACCEPTED — standing rule for every cosmix GUI app (ctk-based).
- **Decision authority:** Mark (explicit, 2026-07-22).
- **Trigger:** starting the theming work (2026-07-22 theming plan — Track A shipped, plan retired 2026-07-23, git history; Track B residue in `_plan/2026-07-23-consolidated-backlog.md`)
  forced the fork: should cosmix apps respect the host's freedesktop icon/cursor
  themes + system fonts, or is the curated own-look *the* cosmix identity? The
  font story (bundled TTF vs `fontdb`/`cosmic-text` system discovery) depends on
  the answer, so it is settled here before any font code.
- **Relationship to neighbours:** enables the theming plan; consistent with
  `2026-07-18-amp-as-control-plane.md` (theme becomes a props namespace later,
  not now) and the gui-reshape (bevy-only apps sharing ctk).

---

## Decision

Cosmix GUI apps present a **curated, self-owned visual identity**: ctk's own
colour palette, its own layout metrics, a compiled-in Lucide icon subset, and a
**bundled font** with a known glyph set. They do **not** inherit the host
desktop's GTK/Qt theme, its freedesktop icon/cursor themes, or its
fontconfig-resolved system fonts.

The palette becomes *data* (a strict-data `.mix` theme file with the current
Rust values as the built-in fallback — see the plan), so it is agent-editable
and desktop-wide themeable. "Own-palette" is about **provenance, not
immutability**: the look is defined and shipped by cosmix, then themeable
through cosmix's own channel — never a reflection of host config.

## Why (the three criteria decide it)

- **Reconstructible.** An app's visual state must be reproducible from the app's
  own world (`app_dirs`' "copy one dir, clone the app"). Host-theme inheritance
  makes appearance a function of host GTK/Qt/fontconfig state an agent cannot
  see or carry — non-determinism that directly fights the criterion the whole
  substrate is built on.
- **Legible.** A version-locked palette + bundled font is a known, queryable
  quantity. "Whatever the host resolved" is not.
- **Identity.** The audio-console look (deep-navy surfaces, indigo-lavender
  controls, traffic-light meters) *is* cosmix; the KDE-pun name points the same
  way. Cosmix is an AI-first substrate, not a citizen of the host DE.

## Consequences

- **Fonts are BUNDLED, not discovered.** Embed a chosen TTF (with a verified
  glyph set) as a ctk asset and make it the app-wide default. No `fontdb` /
  `cosmic-text` system font discovery in v1.
  - *Current state (verified 2026-07-22):* ctk pins Bevy 0.19 with
    `default-features=false`; Bevy core therefore ships **no** font on its own.
    Two fonts leak in today: (1) Bevy's `default_font` feature is on
    *transitively*, embedding a **minimal Fira Mono subset** into `bevy_text` —
    this is what `TextFont::from_font_size(..)` (font handle left at default)
    resolves to for ctk's hand-spawned `Text`; (2) `bevy_feathers` embeds the
    **full Fira family** (Sans + Mono-Medium) and applies it to *its own*
    widgets via `InheritableFont`. So ctk text and Feathers controls silently
    use different coverage. The minimal subset is why non-ASCII (`·`, media
    glyphs) renders as tofu on-device
    (`feedback_fusion_ui_font_ascii_only`). The bundled-font step replaces
    this split with one coverage-checked cosmix typeface set as the root
    `InheritableFont`.
- **Icons stay the compiled-in curated Lucide subset** (already the case); no
  freedesktop icon-theme lookup.
- **Narrow escape hatch, opt-in only.** A future "borrow the host accent colour"
  token override is permissible as an explicit, per-token opt-in. Host-theme
  *as default* is not — the default is always cosmix's own palette.

## Shared with the web (added 2026-07-22)

The cosmix palette *model* is shared verbatim across the web design system
(`~/.gh/dcs.spa`, deployed `webd` `site.css`) and native (ctk): **OKLCH**, where
a scheme is a hue and only the accent's L/C is materially per-scheme. The six
schemes are common to both — **Ocean** (220, default), **Crimson** (25),
**Stone** (60), **Forest** (150), **Sunset** (45), **Mono** (greyscale +
coloured status). Lucide icons and the base/site (structure/palette) layering
are shared too. What does *not* unify: the domain token sets (audio console vs
web page) and the delivery (CSS cascade vs Feathers `UiTheme` rebuild). Unify
the method, the schemes, the icons, the layering — not the vocabularies. (ctk
0.16.0 transcribes the web values; the console default is Ocean-dark.)

Follow-up flagged: `dcs.spa` (the canonical web reference) and the deployed
`webd` assets have **drifted** — a web-side reconciliation, separate from this.

## What this is NOT

Not a rejection of theming — the opposite. The palette is becoming data so it
can be themed *through cosmix's own channel* (file now, ABP props later). This
ADR only fixes the **source of truth**: cosmix, not the host.
