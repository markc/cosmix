# cosmix-dialog Rewrite — Session Journal 2026-04-02

## What Was Done

### Completed Renderers (all 10 Tier 1 dialog types)
- **render/message.rs** — Info/Warning/Error with icon + OK button
- **render/question.rs** — Yes/No with optional Cancel
- **render/input.rs** — Entry + Password with keyboard handling
- **render/text_viewer.rs** — TextViewer (read-only scrollable) + TextInput (multi-line editor)
- **render/choice.rs** — ComboBox, CheckList, RadioList
- **render/progress.rs** — Progress bar with stdin line reader (channel-based)
- **render/form.rs** — Multi-field structured form (8 field kinds)

### Architecture Decisions
- **Shared MenuBar** from cosmix-lib-ui replaces custom TitleBar — same caption buttons as all cosmix apps
- **AlertDialog CSS pattern** (`alert-dialog-*` classes) from dx-components for consistent structure
- **3-section layout**: MenuBar (dark), Content (centered), Footer (dark, pinned bottom)
- **dx components** added via `dx components add alert_dialog` — style.css customized for full-window dialogs
- **Stdout output** fixed: print result before `process::exit()` (was dead code after launch)
- **JSON output** works via `--json` flag with structured `DialogResult`

### MenuBar Improvements (cosmix-lib-ui, affects all apps)
- Height increased from 1.75rem to **2rem**
- Base font-size set to **1rem**
- Menu trigger font bumped to 0.8125rem, height to 1.625rem

### Build/Infra
- dioxus crates updated to 0.7.4 to match dx CLI
- `MESA_LOG_LEVEL=error` added to `init_linux_env()` to suppress Intel Xe warnings
- `document::Stylesheet { href: asset!("/assets/tailwind.css") }` required for dx release builds
- `with_min_inner_size()` and `with_max_inner_size()` added to window config

### Dioxus Native Renderer Experiment
- `dx serve --renderer native` works with `WGPU_BACKEND=vulkan`
- Renders UI but click events don't fire (alpha status)
- Intel Xe GPU needs vulkan backend (gles deadlocks in Vello shader init)
- Not ready for production use

## Key Findings

### cosmic-comp 240px Minimum Height
- Hardcoded in `floating/mod.rs`: `min_size().unwrap_or((320, 240).into())`
- WebKitGTK also enforces ~200-250px minimum content height independently
- `with_min_inner_size()` / `with_max_inner_size()` don't help (GTK widget requisition overrides)
- **Solution**: `gtk-layer-shell` crate for overlay layer surfaces (bypasses toplevel min_size)
- Layer surfaces are semantically correct for transient dialogs (zenity pattern)

### WebKitGTK CSS Rules Confirmed
- CSS custom properties with `var(--name, fallback)` work when `use_theme_css()` injects them
- SVG inside `dangerous_inner_html` cannot use Tailwind classes — use `width`/`height` attributes
- `document::Stylesheet` must be included for compiled Tailwind CSS to load in release builds
- `border: rgba(128,128,128,0.4)` safe border pattern still holds

## Remaining Work
- [ ] Dark theme: currently rendering light — investigate `use_theme_css()` default
- [ ] Layer-shell approach for compact dialogs (< 240px)
- [ ] amp.rs — AMP service mode stub
- [ ] Form CLI subcommand (currently API/AMP-only)
- [ ] Scale, Calendar, Notification renderers (Tier 2)
- [ ] Test all dialog modes end-to-end
- [ ] Copy to ~/.local/bin with assets directory
