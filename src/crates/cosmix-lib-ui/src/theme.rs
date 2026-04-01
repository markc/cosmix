//! OKLCH CSS custom property theme system.
//!
//! Generates a complete set of CSS custom properties from a single hue angle
//! plus a dark/light toggle. All cosmix apps inject the output of `generate_css()`
//! via `document::Style` and reference `var(--bg-primary)` etc.

/// Parameters that fully determine the visual theme.
#[derive(Clone, Debug)]
pub struct ThemeParams {
    /// OKLCH hue angle 0–360.
    pub hue: f32,
    /// Dark mode (true) or light mode (false).
    pub dark: bool,
    /// Base font size in pixels.
    pub font_size: u16,
}

impl Default for ThemeParams {
    fn default() -> Self {
        Self {
            hue: 220.0,
            dark: true,
            font_size: 16,
        }
    }
}

/// Generate the complete CSS custom property block for the given theme params.
pub fn generate_css(p: &ThemeParams) -> String {
    let h = p.hue;
    let fs = p.font_size;
    let fs_sm = fs.saturating_sub(2);
    let fs_lg = fs + 2;

    let (bg1, bg2, bg3) = if p.dark {
        (
            oklch(0.12, 0.015, h),
            oklch(0.16, 0.020, h),
            oklch(0.22, 0.025, h),
        )
    } else {
        (
            oklch(0.98, 0.008, h),
            oklch(0.96, 0.012, h),
            oklch(0.92, 0.018, h),
        )
    };

    let (fg1, fg2, fg3) = if p.dark {
        (
            oklch(0.95, 0.020, h),
            oklch(0.75, 0.050, h),
            oklch(0.55, 0.040, h),
        )
    } else {
        (
            oklch(0.25, 0.060, h),
            oklch(0.40, 0.080, h),
            oklch(0.50, 0.060, h),
        )
    };

    let (accent, accent_hover, accent_fg, accent_subtle, accent_glow) = if p.dark {
        (
            oklch(0.75, 0.12, h),
            oklch(0.85, 0.10, h),
            oklch(0.15, 0.04, h),
            oklch(0.25, 0.04, h),
            oklch_a(0.75, 0.12, h, 0.4),
        )
    } else {
        (
            oklch(0.55, 0.12, h),
            oklch(0.45, 0.14, h),
            oklch(0.98, 0.01, h),
            oklch(0.90, 0.04, h),
            oklch_a(0.55, 0.12, h, 0.3),
        )
    };

    let (border, border_muted) = if p.dark {
        (oklch(0.30, 0.03, h), oklch(0.22, 0.02, h))
    } else {
        (oklch(0.80, 0.02, h), oklch(0.88, 0.015, h))
    };

    // Semantic colours are hue-independent
    let success = oklch(0.55, 0.15, 145.0);
    let danger = oklch(0.55, 0.20, 25.0);
    let warning = oklch(0.70, 0.15, 85.0);

    // Scrollbar colours
    let (scroll_thumb, scroll_hover) = if p.dark {
        (oklch(0.30, 0.02, h), oklch(0.38, 0.025, h))
    } else {
        (oklch(0.75, 0.015, h), oklch(0.65, 0.02, h))
    };

    format!(
        r#":root {{
  --bg-primary: {bg1};
  --bg-secondary: {bg2};
  --bg-tertiary: {bg3};
  --fg-primary: {fg1};
  --fg-secondary: {fg2};
  --fg-muted: {fg3};
  --accent: {accent};
  --accent-hover: {accent_hover};
  --accent-fg: {accent_fg};
  --accent-subtle: {accent_subtle};
  --accent-glow: {accent_glow};
  --border: {border};
  --border-muted: {border_muted};
  --success: {success};
  --danger: {danger};
  --warning: {warning};
  --font-size: {fs}px;
  --font-size-sm: {fs_sm}px;
  --font-size-lg: {fs_lg}px;
  --font-mono: 'JetBrains Mono', 'Fira Code', monospace;
  --font-sans: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif;
  --radius-sm: 4px;
  --radius-md: 6px;
  --radius-lg: 8px;
  --duration-fast: 150ms;
  --duration-base: 200ms;
  /* dx-components bridge — maps their vars to our OKLCH theme */
  --primary-color: {bg1};
  --primary-color-1: {bg1};
  --primary-color-2: {bg2};
  --primary-color-3: {bg2};
  --primary-color-4: {bg3};
  --primary-color-5: {bg3};
  --primary-color-6: {border};
  --primary-color-7: {border_muted};
  --secondary-color: {fg1};
  --secondary-color-1: {fg1};
  --secondary-color-2: {accent};
  --secondary-color-3: {fg2};
  --secondary-color-4: {fg1};
  --secondary-color-5: {fg3};
  --secondary-color-6: {border};
  --focused-border-color: {accent};
  --primary-error-color: {danger};
  --secondary-error-color: {danger};
  --contrast-error-color: {fg1};
  --primary-success-color: {success};
  --secondary-success-color: {success};
  --primary-warning-color: {warning};
  --secondary-warning-color: {warning};
  --primary-info-color: {bg3};
  --secondary-info-color: {fg2};
  --sidebar-background: {bg2};
  --sidebar-foreground: {fg1};
  --sidebar-border: {border};
  --sidebar-accent: {accent_subtle};
  --sidebar-accent-foreground: {fg1};
  --sidebar-ring: {accent};
}}
@keyframes cmx-fade-in {{
  from {{ opacity: 0; }}
  to {{ opacity: 1; }}
}}
html, body, #main {{
  margin: 0; padding: 0;
  width: 100%; height: 100%;
  overflow: hidden;
  font-size: {fs}px;
  background: var(--bg-primary);
  color: var(--fg-primary);
}}
#main {{
  animation: cmx-fade-in 200ms ease-out;
}}
::-webkit-scrollbar {{ width: 8px; }}
::-webkit-scrollbar-track {{ background: transparent; }}
::-webkit-scrollbar-thumb {{ background: {scroll_thumb}; border-radius: 4px; }}
::-webkit-scrollbar-thumb:hover {{ background: {scroll_hover}; }}
*, *::before, *::after {{ box-sizing: border-box; }}
button, input, select, textarea {{
  all: unset;
  font: inherit;
  color: inherit;
}}
button {{
  cursor: pointer;
  display: inline-flex;
  align-items: center;
  justify-content: center;
}}
input, textarea {{
  cursor: text;
}}
button:hover {{ filter: brightness(1.15); }}
input:focus {{ border-color: var(--accent) !important; }}
textarea {{ caret-color: var(--accent); }}
"#
    )
}

fn oklch(l: f32, c: f32, h: f32) -> String {
    format!("oklch({:.0}% {:.3} {:.1})", l * 100.0, c, h)
}

fn oklch_a(l: f32, c: f32, h: f32, a: f32) -> String {
    format!("oklch({:.0}% {:.3} {:.1} / {:.1})", l * 100.0, c, h, a)
}