# Dioxus UI Foundation — The Path to Professional Consistency

**Date:** 2026-03-31
**Status:** Phase 1 complete (2026-04-01) — deps added, Tailwind theme created, primitives re-exported. Phase 2 next: build components.
**Priority:** Critical — blocks all future UI work

## The Problem

Two hours lost fighting a single toggle button and row borders in cosmix-dopus. The root causes:

1. **WebKitGTK renders differently from Chrome/Firefox** — CSS variables can resolve to transparent/empty, collapsing borders to 0px and causing layout shifts
2. **No component library** — every app hand-writes inline styles, repeating the same mistakes
3. **No CSS methodology** — inline style strings are untestable, unmaintainable, and WebKitGTK-hostile
4. **Native form controls** — WebKitGTK applies GTK theme styling to `<button>`, `<input>`, `<select>` that fights CSS

These aren't cosmix bugs — they're structural problems that will compound with every new app.

## What We Discovered Tonight

### WebKitGTK Gotchas (hard-won knowledge)

| Issue | Cause | Fix |
|-------|-------|-----|
| Border disappears on background change | `var(--border)` resolves to transparent in some states | Use concrete `rgba(128,128,128,0.4)` for borders that must survive state changes |
| Button changes size on toggle | WebKitGTK native form control rendering | `all: unset` globally on form elements, or use `div` + `onclick` |
| Drag ghost image is huge | Native HTML5 drag on Wayland | Don't use `draggable`; implement mousedown/mousemove/mouseup instead |
| Drag crashes Wayland connection | WebKitGTK HTML5 drag protocol bug | Avoid native drag entirely on Wayland |
| Font renders ~100 weight units heavier | WebKitGTK font weight bug | Specify lighter weights than intended (300 for 400 appearance) |
| `px` units inconsistent | WebKitGTK vs Chromium rendering difference | Prefer `rem` or `%` where possible |
| `backdrop-filter` broken | Not fully implemented in WebKitGTK | Don't use it |
| CSS animations cause blur | Compositing bug | Keep animations simple, use transforms only |
| Black screen on launch | GPU compositing | `WEBKIT_DISABLE_COMPOSITING_MODE=1` (already set) |

### CSS Rules That Prevent Layout Shift

1. **Never change border properties on state change** — only change `background` and `color`
2. **Use `rgba()` concrete values** for borders, never CSS variables that might resolve to empty
3. **`box-sizing: border-box` on everything** — already in our global reset
4. **`all: unset` on all form elements** — already in our global reset in theme.rs
5. **Use `div` + `onclick` for app chrome buttons** — bypasses all WebKitGTK form styling
6. **Fixed height rows** with consistent padding — no content-dependent sizing

## The Solution: Three-Layer Architecture

### Layer 1: Global CSS Reset (DONE)

Already in `cosmix-lib-ui/src/theme.rs`:

```css
*, *::before, *::after { box-sizing: border-box; }
button, input, select, textarea {
  all: unset;
  font: inherit;
  color: inherit;
}
```

### Layer 2: Tailwind CSS v4 Integration (NEW)

Dioxus 0.7 has first-class Tailwind support — auto-detects `tailwind.css` in project root and runs the watcher during `dx serve`. Benefits:

- **Utility classes replace inline styles** — `class: "flex items-center px-2 border-b border-gray-500/40"` instead of `style: "display:flex; align-items:center; padding:0 8px; border-bottom:1px solid rgba(128,128,128,0.4);"`
- **Tested, predictable values** — Tailwind's spacing/color scales are battle-tested
- **Dark mode built in** — `dark:bg-gray-800` just works
- **OKLCH theme integration** — custom properties as Tailwind theme values
- **Consistent across all apps** — same classes = same rendering

### Layer 3: Component Library (NEW)

#### Adopt: dioxus-primitives (official, 28 components)

URL: https://github.com/DioxusLabs/components

28 unstyled, accessible, keyboard-navigable Radix-inspired primitives:

- **Already have:** Accordion, Alert Dialog, Avatar, Calendar, Checkbox, Collapsible, Context Menu, Dialog, Dropdown Menu, Hover Card, Label, Menubar, Navigation Menu, Popover, Progress, Radio Group, Scroll Area, Select, Separator, Slider, Switch, Tabs, Toast, Toggle, Toggle Group, Toolbar, Tooltip
- **Unstyled** — we apply our OKLCH theme via Tailwind classes
- **ARIA accessible** — keyboard navigation and screen reader support included
- **Dioxus 0.7 compatible**

#### Build in cosmix-lib-ui: Missing Widgets

| Widget | Priority | Why |
|--------|----------|-----|
| **DataTable** | Critical | cosmix-mail, cosmix-dopus, cosmix-dns, cosmix-backup all need sortable/paginated tables |
| **TreeView** | Critical | cosmix-files, cosmix-dopus, cosmix-shell sidebar all need hierarchical navigation |
| **SplitPane** | High | cosmix-dopus dual panes, cosmix-shell sidebar/content, cosmix-edit split view |
| **FileRow** | High | Battle-tested row component with guaranteed stable borders |
| **ButtonBank** | Medium | cosmix-dopus button grid, reusable toolbar pattern |
| **Terminal** | Medium | cosmix-shell needs an embedded terminal |
| **StatusBar** | Low | Consistent bottom bar across all apps |

#### Adopt: Icon Libraries

- **lucide-dioxus** (1000+ icons, updated 2026-03, Dioxus 0.7) — primary icon set
- **dioxus-free-icons** (106K downloads, Font Awesome/Bootstrap/Heroicons) — fallback

#### Adopt: Other Ecosystem Crates

- **dioxus-charts** — SVG charts for cosmix-mon dashboards
- **dioxus-query** — async cached state management (TanStack Query pattern)
- **dioxus-sdk** — clipboard, notifications, storage, window_size

## Implementation Plan

### Phase 1: Foundation (1 session)

1. **Add Tailwind CSS v4** to the workspace
   - Create `tailwind.css` in workspace root
   - Configure OKLCH custom properties as Tailwind theme
   - Test with cosmix-dopus (simplest visual app to validate)

2. **Add `dioxus-primitives`** to cosmix-lib-ui deps
   - Re-export useful primitives
   - Test Toggle, Tabs, Dialog in cosmix-dopus

3. **Add `lucide-dioxus`** to cosmix-lib-ui
   - Replace current SVG icon strings with proper icon components

4. **Update CLAUDE.md** with WebKitGTK rules and component guidelines

### Phase 2: Core Widgets (1-2 sessions)

5. **Build DataTable component** in cosmix-lib-ui
   - Sortable columns (click header)
   - Fixed header with scrollable body
   - Row selection (single + multi)
   - Battle-tested borders (rgba, never CSS vars)
   - Pagination
   - Used by: cosmix-mail (inbox), cosmix-dopus (file list), cosmix-dns, cosmix-backup

6. **Build TreeView component** in cosmix-lib-ui
   - Expand/collapse with indent
   - Icon per node (folder/file/custom)
   - Selection + keyboard navigation
   - Used by: cosmix-files, cosmix-dopus, cosmix-shell sidebar

7. **Build SplitPane component** in cosmix-lib-ui
   - Horizontal and vertical split
   - Draggable divider (mousedown/move/up, NOT native drag)
   - Min/max constraints
   - Used by: cosmix-dopus, cosmix-shell, cosmix-edit

### Phase 3: Migrate Existing Apps (1-2 sessions)

8. **Rewrite cosmix-dopus** using DataTable + SplitPane + ButtonBank
   - The test case for the entire component system
   - Should be ~400 lines instead of ~900

9. **Rewrite cosmix-files** using TreeView + DataTable
   - Simple single-pane becomes TreeView sidebar + DataTable content

10. **Update cosmix-edit, cosmix-view** with Tailwind classes
    - Replace inline styles with utility classes
    - Consistent borders, spacing, colors

### Phase 4: The Workbench Shell (future)

11. **cosmix-shell as a Workbench**
    - Absorbed apps as "windows" within the shell WebView
    - Icon desktop (Workbench-style) with drag-and-drop (mousedown/move/up)
    - Taskbar/dock at bottom
    - Floating window management (cosmix-lib-ui already has FloatingWindow)
    - The DCS vision: desktop + WASM thin client, same code

## What This Enables

With the foundation in place:

- **100 apps** can be built without fighting CSS once — use DataTable, TreeView, SplitPane, Toggle, Dialog, etc
- **Desktop and WASM** render identically — Tailwind + components are platform-agnostic
- **Dark/light mode** works everywhere — Tailwind + OKLCH theme
- **Accessibility** comes free — dioxus-primitives handles ARIA + keyboard
- **WebKitGTK quirks are isolated** in the foundation layer — app code never touches raw borders or form elements
- **The Workbench shell** becomes a composition of tested components, not a hand-styled HTML page

## Key URLs

- DioxusLabs/components: https://github.com/DioxusLabs/components
- Components preview: https://dioxuslabs.github.io/components/
- Tailwind + Dioxus: https://dioxuslabs.com/learn/0.7/essentials/ui/styling/
- lucide-dioxus: https://crates.io/crates/lucide-dioxus
- dioxus-charts: https://crates.io/crates/dioxus-charts
- dioxus-query: https://crates.io/crates/dioxus-query
- WebKitGTK font bug: https://github.com/nicedoc/webkitgtk-bugs

## The Vision

Amiga Workbench had a consistent widget set that every app used — standardized by Intuition and GadTools. Every button looked the same, every list behaved the same, every requester was identical. That consistency made the platform feel professional despite running on 512KB of RAM.

Cosmix needs the same thing. `dioxus-primitives` + Tailwind + cosmix-lib-ui widgets = the modern GadTools. Build it once, use it everywhere, never fight a 1px border again.
