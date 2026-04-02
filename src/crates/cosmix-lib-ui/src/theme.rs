//! Theme configuration and CSS custom property defaults.
//!
//! Defines the CSS custom properties that all cosmix apps and components
//! reference via `var(--bg-primary)`, `var(--accent)`, etc. These are
//! injection points for the future cosmix-confd global theming via AMP.
//!
//! The previous OKLCH dynamic generation was removed because it broke the
//! dx-components `--dark`/`--light` toggle pattern. See
//! `_doc/2026-04-02-oklch-theme-lessons.md` for details.
//!
//! Current approach: static CSS defaults with good fallback values.
//! cosmix-confd will override these at runtime via AMP → `use_theme_css()`.

/// Parameters that determine the visual theme.
/// Will be extended when cosmix-confd drives theming via AMP.
#[derive(Clone, Debug)]
pub struct ThemeParams {
    /// Dark mode (true) or light mode (false).
    pub dark: bool,
    /// Base font size in pixels.
    pub font_size: u16,
}

impl Default for ThemeParams {
    fn default() -> Self {
        Self {
            dark: true,
            font_size: 16,
        }
    }
}

/// Generate the CSS custom property block for the given theme params.
///
/// Outputs a `:root { ... }` block defining all `--bg-*`, `--fg-*`, `--accent-*`,
/// `--border-*`, `--danger`, `--success`, `--warning` variables plus base styles.
///
/// These are static values (not computed OKLCH). The variable names are the
/// injection points that cosmix-confd will override at runtime.
pub fn generate_css(p: &ThemeParams) -> String {
    let fs = p.font_size;
    let fs_sm = fs.saturating_sub(2);
    let fs_lg = fs + 2;

    // Select palette based on dark/light mode
    let (bg1, bg2, bg3) = if p.dark {
        ("#030712", "#111827", "#1f2937")  // gray-950, gray-900, gray-800
    } else {
        ("#ffffff", "#f9fafb", "#f3f4f6")  // white, gray-50, gray-100
    };

    let (fg1, fg2, fg3) = if p.dark {
        ("#f3f4f6", "#d1d5db", "#6b7280")  // gray-100, gray-300, gray-500
    } else {
        ("#111827", "#374151", "#6b7280")  // gray-900, gray-700, gray-500
    };

    let (accent, accent_hover, accent_fg, accent_subtle) = if p.dark {
        ("#3b82f6", "#60a5fa", "#030712", "#1e3a5f")  // blue-500, blue-400, gray-950, custom
    } else {
        ("#2563eb", "#1d4ed8", "#ffffff", "#dbeafe")  // blue-600, blue-700, white, blue-100
    };

    let (border, border_muted) = if p.dark {
        ("#374151", "#1f2937")  // gray-700, gray-800
    } else {
        ("#d1d5db", "#e5e7eb")  // gray-300, gray-200
    };

    let (scroll_thumb, scroll_hover) = if p.dark {
        ("#374151", "#4b5563")  // gray-700, gray-600
    } else {
        ("#9ca3af", "#6b7280")  // gray-400, gray-500
    };

    // Semantic colours — same in light and dark
    let success = "#22c55e";  // green-500
    let danger = "#ef4444";   // red-500
    let warning = "#eab308";  // yellow-500

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
  --accent-glow: {accent}66;
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
  --radius-sm: 0.25rem;
  --radius-md: 0.375rem;
  --radius-lg: 0.5rem;
  --duration-fast: 150ms;
  --duration-base: 200ms;
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
::-webkit-scrollbar {{ width: 0.5rem; }}
::-webkit-scrollbar-track {{ background: transparent; }}
::-webkit-scrollbar-thumb {{ background: {scroll_thumb}; border-radius: 0.25rem; }}
::-webkit-scrollbar-thumb:hover {{ background: {scroll_hover}; }}
*, *::before, *::after {{ box-sizing: border-box; }}
"#
    )
}
