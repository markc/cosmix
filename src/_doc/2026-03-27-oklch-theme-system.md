# cosmix-ui OKLCH Theme System

**Date:** 2026-03-27
**Status:** Planned — global font_size is implemented, color theme is next
**Depends on:** `cosmix-config` (GlobalSettings), `cosmix-ui` theme module

---

## Background

The existing cosmix-ui theme is 12 compile-time `const &str` CSS hex values. They work but
have two problems:

1. **Not runtime-switchable** — changing the theme requires recompiling
2. **Not derivable** — each of the 12 values is hand-authored with no mathematical relationship
   between them, making it impossible to generate light/dark variants or accent colour shifts
   programmatically

This document describes the replacement: a CSS custom property system using OKLCH colour values
derived from a single hue angle, with live reload via the AMP mesh.

---

## Why OKLCH

OKLCH (`oklch(L C H)`) is a perceptually uniform CSS colour space (CSS Color Level 4):

- **L** = lightness 0–1 (0 = black, 1 = white)
- **C** = chroma (colourfulness, approximately 0–0.4)
- **H** = hue angle 0–360

"Perceptually uniform" means equal numeric steps in L look equal to human eyes, unlike HSL
where yellow at 50% lightness looks far brighter than blue at 50% lightness.

**Browser support:** Chrome 111+, Firefox 113+, Safari 15.4+, WebKitGTK on Arch/CachyOS ✓

**Key advantages for cosmix-ui:**
- An entire dark or light theme is derived from **one hue angle** — change H, get a new theme
- Light mode = flip L values — no second set of colour definitions needed
- Hover/active/disabled states = ±L — no hardcoding extra colours
- Accessible contrast is checkable with L difference arithmetic
- Alpha transparency: `oklch(L C H / 0.3)` — clean syntax

---

## The Five Named Themes

Sourced from [dcs.spa](https://dcs.spa/) analysis. Each is purely a hue angle:

| Name | Hue (H) | Character |
|---|---|---|
| Ocean | 220 | Cyan-blue, default |
| Crimson | 25 | Bold red, high energy |
| Stone | 60 | Warm neutral, minimal |
| Forest | 150 | Natural green, balanced |
| Sunset | 45 | Warm orange-amber |

Named themes are **presets** for `theme_hue` in GlobalSettings. Users can also dial any hue
0–360 directly. The name is just a label for the preset value.

---

## The Complete Colour Derivation (Dark Mode)

All dark mode colours for any theme are derived from `H` with fixed lightness/chroma offsets:

```
Background layers (darkest to lightest):
  --bg-primary:    oklch(12%  0.015  H)    ← base window background
  --bg-secondary:  oklch(16%  0.02   H)    ← surfaces (sidebars, cards)
  --bg-tertiary:   oklch(22%  0.025  H)    ← elevated elements, inputs

Foreground / text:
  --fg-primary:    oklch(95%  0.02   H)    ← body text
  --fg-secondary:  oklch(75%  0.05   H)    ← secondary labels
  --fg-muted:      oklch(55%  0.04   H)    ← placeholder, disabled text

Accent (interactive elements):
  --accent:        oklch(75%  0.12   H)    ← buttons, links, focus rings
  --accent-hover:  oklch(85%  0.10   H)    ← hover state
  --accent-fg:     oklch(15%  0.04   H)    ← text on accent background
  --accent-subtle: oklch(25%  0.04   H)    ← tinted background
  --accent-glow:   oklch(75%  0.12   H / 0.4)  ← glow effects

Borders:
  --border:        oklch(30%  0.03   H)    ← visible borders
  --border-muted:  oklch(22%  0.02   H)    ← subtle borders

Semantic (hue-independent):
  --success:       oklch(55%  0.15   145)  ← always green
  --danger:        oklch(55%  0.20   25)   ← always red
  --warning:       oklch(70%  0.15   85)   ← always yellow
```

**Light mode** uses flipped L values:

```
  --bg-primary:    oklch(98%  0.008  H)
  --bg-secondary:  oklch(96%  0.012  H)
  --bg-tertiary:   oklch(92%  0.018  H)
  --fg-primary:    oklch(25%  0.06   H)
  --fg-secondary:  oklch(40%  0.08   H)
  --fg-muted:      oklch(50%  0.06   H)
  --accent:        oklch(55%  0.12   H)    ← lower L for contrast on light bg
  ... etc.
```

Two modes × infinite hue angles, zero hand-authored colour values.

---

## CSS Custom Properties: The Architecture

Instead of passing colour values through Rust into every inline style attribute, cosmix-ui
injects a **single `<style>` block** into the document head defining all CSS custom properties.
Every app element then references `var(--bg-primary)` etc.

### The generator function

```rust
// crates/cosmix-ui/src/theme.rs

pub struct ThemeParams {
    pub hue: f32,        // 0–360
    pub dark: bool,      // true = dark mode
    pub font_size: u16,  // base px
}

pub fn generate_css(p: &ThemeParams) -> String {
    let (bg1, bg2, bg3, fg1, fg2, fg3) = if p.dark {
        (
            oklch(0.12, 0.015, p.hue),
            oklch(0.16, 0.020, p.hue),
            oklch(0.22, 0.025, p.hue),
            oklch(0.95, 0.020, p.hue),
            oklch(0.75, 0.050, p.hue),
            oklch(0.55, 0.040, p.hue),
        )
    } else {
        (
            oklch(0.98, 0.008, p.hue),
            oklch(0.96, 0.012, p.hue),
            oklch(0.92, 0.018, p.hue),
            oklch(0.25, 0.060, p.hue),
            oklch(0.40, 0.080, p.hue),
            oklch(0.50, 0.060, p.hue),
        )
    };
    // ... accent, border etc.

    format!(r#"
:root {{
  --bg-primary:    {bg1};
  --bg-secondary:  {bg2};
  --bg-tertiary:   {bg3};
  --fg-primary:    {fg1};
  --fg-secondary:  {fg2};
  --fg-muted:      {fg3};
  --accent:        {accent};
  --accent-hover:  {accent_hover};
  --border:        {border};
  --border-muted:  {border_muted};
  --font-size:     {fs}px;
  --font-size-sm:  {fs_sm}px;
  --font-size-lg:  {fs_lg}px;
  --font-mono:     'JetBrains Mono', 'Fira Code', monospace;
  --font-sans:     -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif;
}}
html, body {{ font-size: {fs}px; }}
"#)
}

fn oklch(l: f32, c: f32, h: f32) -> String {
    format!("oklch({:.0}% {:.3} {:.1})", l * 100.0, c, h)
}
```

### How apps use it

In each app's `app()` function:

```rust
// At the top of app():
let css = cosmix_ui::theme::generate_css(&cosmix_ui::theme::current_params());

// In rsx!:
document::Style { "{css}" }

// All style strings use variables:
div {
    style: "background: var(--bg-primary); color: var(--fg-primary);",
    ...
}
```

The `current_params()` function reads from `GlobalSettings` (via the `THEME_PARAMS` global
signal). A theme change re-generates and re-injects the CSS block. The CSS engine resolves all
`var()` references instantly — no per-element Rust updates needed.

### The global signal

```rust
// crates/cosmix-ui/src/theme.rs

pub static THEME_PARAMS: GlobalSignal<ThemeParams> = Signal::global(|| {
    cosmix_config::store::load()
        .map(|s| ThemeParams {
            hue: s.global.theme_hue,
            dark: s.global.theme_dark,
            font_size: s.global.font_size,
        })
        .unwrap_or_else(|_| ThemeParams::default())
});

pub fn current_params() -> ThemeParams {
    THEME_PARAMS.read().clone()
}
```

---

## GlobalSettings Changes

Current `GlobalSettings` (as implemented 2026-03-27):

```toml
[global]
font_size = 14
theme = "dark"
```

Target:

```toml
[global]
font_size = 14
theme_hue = 220.0      # Ocean by default
theme_dark = true      # dark mode
```

Named theme presets are helper functions, not stored values:

```rust
pub fn preset_hue(name: &str) -> f32 {
    match name {
        "ocean"   => 220.0,
        "crimson" => 25.0,
        "stone"   => 60.0,
        "forest"  => 150.0,
        "sunset"  => 45.0,
        _         => 220.0,
    }
}
```

cosmix-settings shows a theme preset picker (dropdown or swatches) that sets `theme_hue` to
the preset value, plus a hue slider for custom values and a dark/light toggle.

---

## CSS Variable Naming (dcs.spa convention)

Migrating from cosmix-ui's current names to the dcs.spa convention:

| Old const | New CSS variable | Notes |
|---|---|---|
| `BG_BASE` | `var(--bg-primary)` | |
| `BG_SURFACE` | `var(--bg-secondary)` | |
| `BG_ELEVATED` | `var(--bg-tertiary)` | |
| `TEXT_PRIMARY` | `var(--fg-primary)` | |
| `TEXT_SECONDARY` | `var(--fg-secondary)` | |
| `TEXT_MUTED` | `var(--fg-muted)` | |
| `TEXT_DIM` | `var(--fg-muted)` | fold into muted |
| `BORDER_DEFAULT` | `var(--border)` | |
| `BORDER_SUBTLE` | `var(--border-muted)` | |
| `ACCENT_BLUE` | `var(--accent)` | |
| — | `var(--accent-hover)` | new |
| — | `var(--accent-subtle)` | new |

---

## Additional CSS Variables (from dcs.spa)

Worth adopting alongside the colour variables:

```css
:root {
  /* Spacing scale */
  --space-1: 0.25rem;  --space-2: 0.5rem;   --space-3: 0.75rem;
  --space-4: 1rem;     --space-6: 1.5rem;   --space-8: 2rem;

  /* Border radius */
  --radius-sm: 4px;   --radius-md: 6px;
  --radius-lg: 8px;   --radius-full: 9999px;

  /* Shadows */
  --shadow-sm: 0 1px 2px oklch(0% 0 0 / 0.15);
  --shadow-md: 0 4px 8px oklch(0% 0 0 / 0.2);
  --shadow-lg: 0 8px 24px oklch(0% 0 0 / 0.25);

  /* Transitions */
  --ease-out:    cubic-bezier(0.33, 1, 0.68, 1);
  --ease-spring: cubic-bezier(0.34, 1.56, 0.64, 1);
  --duration-fast: 150ms;  --duration-base: 200ms;  --duration-slow: 300ms;
}
```

These are constant across all themes and can live in cosmix-ui's static CSS block.

---

## Live Reload

### cosmix-edit (hub-connected service)

Already registers `config.watch` on startup. The `config.changed` handler:

```rust
"config.changed" => {
    if let Ok(settings) = cosmix_config::store::load() {
        *FONT_SIZE.write() = settings.global.font_size;
        // Add:
        *cosmix_ui::theme::THEME_PARAMS.write() = ThemeParams {
            hue: settings.global.theme_hue,
            dark: settings.global.theme_dark,
            font_size: settings.global.font_size,
        };
    }
    Ok(r#"{"status":"ok"}"#.to_string())
}
```

### Other apps (30-second polling)

The existing 30s poll loop in cosmix-files, cosmix-mon, cosmix-view already reloads font_size.
Extend to also reload `THEME_PARAMS`.

### cosmix-shell (when built)

cosmix-shell connects as a named service and registers `config.watch` like cosmix-edit. It is
the primary user-facing surface so it should have hub-based live reload, not polling.

---

## Implementation Phases

### Phase 1 — `generate_css()` function
- Add `ThemeParams` struct to `cosmix-ui/src/theme.rs`
- Implement `generate_css(p: &ThemeParams) -> String`
- Add `THEME_PARAMS: GlobalSignal<ThemeParams>` reading from cosmix-config
- Verify: `cargo check -p cosmix-ui`

### Phase 2 — Update GlobalSettings
- Replace `theme: String` with `theme_hue: f32` and `theme_dark: bool` in cosmix-config
- Update cosmix-settings SECTIONS and field rendering (hue slider, dark toggle, preset picker)
- Verify: `cargo check` workspace-wide

### Phase 3 — Migrate cosmix-edit first
- Replace 12 `const BG_*` / `TEXT_*` with `var(--bg-primary)` etc. in all style strings
- Inject `document::Style { "{generate_css(&current_params())}" }` in `app()`
- Extend `config.changed` handler to update `THEME_PARAMS`
- Build and visually verify dark/light switching

### Phase 4 — Migrate cosmix-files, cosmix-mon, cosmix-view
- Same pattern as Phase 3
- Extend 30s polling loops to update `THEME_PARAMS`

### Phase 5 — cosmix-shell uses system from day one
- cosmix-shell is built with CSS variables from the start
- No legacy `const` colour strings ever introduced

---

## The `palette` Crate (Optional Future Enhancement)

The `palette` crate (pure Rust, WASM-safe) provides `Oklch<f32>` with conversion to/from
`Srgb`. This enables:

- Contrast checking: verify `--fg-primary` on `--bg-primary` meets WCAG AA
- Hover state generation: `accent.lighten(0.1)` instead of hardcoded L offset
- Colour harmony: generate complementary/analogous accent colours
- P3 gamut mapping for wide-colour displays

Adding `palette` is optional and does not change the CSS output format — it would only replace
the hardcoded L/C/H offset arithmetic with type-safe colour math. Not needed for Phase 1–5.

---

## Non-Goals

- **CSS-in-JS**: All CSS is generated as strings and injected via `document::Style`. No
  JavaScript CSS-in-JS libraries, no Tailwind, no PostCSS.
- **Server-side rendering**: Colours are resolved client-side by the CSS engine. No build step.
- **Compile-time themes**: All theming is runtime. No feature flags per theme.
- **cosmic-theme crate**: Not usable — not on crates.io, hard dep on `cosmic-config`, wrong
  output format (Srgba floats, not CSS strings), WASM-incompatible.

---

## Related Documents

- `2026-03-27-cosmix-shell-vision.md` — Shell architecture using this theme system
- `2026-03-26-appmesh-ecosystem-roadmap.md` — Roadmap context
