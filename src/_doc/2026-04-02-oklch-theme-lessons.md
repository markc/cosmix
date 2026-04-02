# OKLCH Theme System — Lessons Learned

Documented 2026-04-02. This captures what we built, why it broke, and what to know before attempting a custom theme system again.

## What We Built

A dynamic OKLCH-based theme system in `cosmix-lib-ui/src/theme.rs` that:

1. Generated all CSS custom properties from a single hue angle (0–360) + dark/light toggle
2. Injected CSS at runtime via `document::eval()` into a `<style id="cosmix-theme">` element
3. Bridged our OKLCH vars to dx-components' expected variable names
4. Updated reactively via a `THEME` global signal polled from config

### The Color Palette

From one hue angle, we generated:
- 3 background levels (bg-primary, bg-secondary, bg-tertiary)
- 3 foreground levels (fg-primary, fg-secondary, fg-muted)
- Accent colors (accent, accent-hover, accent-fg, accent-subtle, accent-glow)
- Borders (border, border-muted)
- Semantic colors (success=145, danger=25, warning=85 — hue-independent)
- Scrollbar colors

All using OKLCH for perceptual uniformity — `oklch(L% C H)` format.

### The Bridge Layer

Mapped our vars to dx-components' expected names:
```css
--primary-color: {bg1};
--primary-color-1 through --primary-color-7
--secondary-color: {fg1};
--secondary-color-1 through --secondary-color-6
--focused-border-color: {accent};
--sidebar-background, --sidebar-foreground, etc.
```

## Why It Broke

### Root Cause: The --dark/--light Toggle

dx-components use a CSS pattern where both light and dark values appear in the same declaration:

```css
background-color: var(--dark, #333) var(--light, #fff);
```

The mechanism:
- When `--dark: initial` → `var(--dark, #333)` returns the **fallback** `#333`
- When `--dark` is **empty string** `""` → `var(--dark, #333)` returns empty, making that half invisible

Our theme.rs set the inactive toggle to `" "` (space character):
```rust
let (dark_toggle, light_toggle) = if p.dark {
    ("initial", " ")  // BUG: space, not empty
} else {
    (" ", "initial")   // BUG: space, not empty
};
```

A space is NOT an empty string in CSS. It resolves to a valid (but wrong) value, producing concatenated garbage like `" " #fff` — invalid CSS.

### Impact

18 of 41 dx-components use the toggle pattern. Broken items included:
- Input borders (box-shadow with toggle)
- Button outline variant (background with toggle)
- Card borders
- Many other components

### The Global `all: unset`

Our theme.rs also included:
```css
button, input, select, textarea { all: unset; font: inherit; color: inherit; }
```

This strips ALL default and inherited styles from form elements, including any dx-component styles applied via class selectors. It made buttons lose their padding, inputs lose their borders, etc.

## What We Verified

- Official Dioxus examples all work perfectly on our WebKitGTK setup
- dx-components preview app renders all 40 components correctly with proper borders, colors, focus states
- The problem was entirely in our bridge layer, not WebKitGTK or the components

## Requirements For Future Reimplementation

If we ever want to bring back custom theming:

1. **Start from working stock Tailwind** — never build a theme system without a proven baseline
2. **Don't set `--dark`/`--light` directly** — use `data-theme` attribute on `<html>` and let dx-components-theme.css handle the toggles
3. **Don't use `all: unset` on form elements** — it destroys dx-component styles
4. **Test with the dx-components preview app first** — render all 40 components and verify visually
5. **Layer changes incrementally** — one variable at a time, testing after each change
6. **Consider CSS layers** — use `@layer` to control cascade order between our styles and dx-components
7. **Empty string vs space matters** — in CSS `var()` fallback patterns, `""` (empty) and `" "` (space) are completely different

## The OKLCH Approach Is Sound

The OKLCH color generation itself was good engineering:
- Perceptually uniform color space
- Single hue angle controls the entire palette
- Light/dark mode is just different lightness/chroma values
- Semantic colors (success/danger/warning) are hue-independent

The problem was never OKLCH — it was the CSS bridge to dx-components. A future implementation should use OKLCH for color generation but apply them through standard Tailwind `@theme` or CSS custom properties that don't conflict with the dx-components toggle pattern.
