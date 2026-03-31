# Tree View Widget Research for iced / libcosmic

Date: 2026-03-07

## Executive Summary

There is **no dedicated tree view widget** in either iced or libcosmic. The gap is well-known (iced issue #209 has been open since 2020). The only working "tree view" in the COSMIC ecosystem is cosmic-edit's project sidebar, which uses libcosmic's `segmented_button::SingleSelectModel` with its built-in `indent` support -- not a custom tree widget. This is a composition approach, not a reusable component.

---

## 1. libcosmic (pop-os/libcosmic)

### Issues & PRs

- **No tree view issues or PRs exist.** Searched issues for "tree", "treeview", "hierarchical", "nested" -- zero results specific to a tree widget.
- Issue #622 ("Api for lazy menu tree") exists but is about menu widgets, not a general tree view.

### Built-in Indent Support in segmented_button

libcosmic's `segmented_button` widget has **native indent support** that enables tree-like display:

```rust
// From src/widget/segmented_button/widget.rs
indent_spacing: 16,  // pixels per indent level

// In layout calculation:
if let Some(indent) = self.model.indent(button) {
    width = f32::from(indent).mul_add(f32::from(self.indent_spacing), width);
}

// In rendering - draws vertical indent guide lines:
if let crate::theme::SegmentedButton::FileNav = self.style && indent > 1 {
    for level in 1..indent {
        renderer.fill_quad(/* vertical line at each indent level */);
    }
}
```

The `segmented_button::Model` supports:
- `.indent(n)` -- set indent level on an entity
- `.indent(id)` -- query indent level
- `.indent_set(id, n)` -- update indent level
- Vertical guide lines rendered for `FileNav` style when indent > 1

This is the **only tree-like rendering capability** in libcosmic today.

### Nav Bar

The nav bar (`cosmic::widget::nav_bar`) uses `segmented_button::SingleSelectModel` internally. It provides sidebar navigation with icons, text, and selection, but is flat by default. The indent support makes it tree-capable when used manually (as cosmic-edit demonstrates).

---

## 2. cosmic-edit (pop-os/cosmic-edit) -- The Reference Implementation

cosmic-edit is the **only COSMIC app with a working file tree**. Its approach is instructive.

### Data Model (`src/project.rs`)

```rust
pub enum ProjectNode {
    Folder { name: String, path: PathBuf, open: bool, root: bool },
    File { name: String, path: PathBuf },
}
```

- Sorting: folders before files, then locale-aware alphabetical
- Icons: `go-down-symbolic` / `go-next-symbolic` for folders, MIME-based icons for files

### Tree Construction (`src/main.rs`)

Uses `nav_model` (a `segmented_button::SingleSelectModel`) with manual position and indent management:

```rust
fn open_folder<P: AsRef<Path>>(&mut self, path: P, mut position: u16, indent: u16) {
    // Walk directory (depth=1), create ProjectNode per entry, sort
    for node in nodes {
        self.nav_model
            .insert()
            .position(position)
            .indent(indent)          // <-- tree depth
            .icon(node.icon(16))
            .text(node.name().to_string())
            .data(node);
        position += 1;
    }
}
```

### Expand/Collapse (`on_nav_select`)

```rust
fn on_nav_select(&mut self, id: nav_bar::Id) -> Task<Message> {
    // Toggle open state
    if let ProjectNode::Folder { open, .. } = node { *open = !*open; }
    // Update icon
    self.nav_model.icon_set(id, node.icon(16));

    if open {
        // Insert children at position+1, indent+1
        self.open_folder(path, position + 1, indent + 1);
    } else {
        // Remove all consecutive items with indent > current
        while let Some(child_id) = self.nav_model.entity_at(position + 1) {
            if self.nav_model.indent(child_id).unwrap_or(0) > indent {
                self.nav_model.remove(child_id);
            } else { break; }
        }
    }
}
```

### Characteristics

| Feature | Status |
|---------|--------|
| Collapsible folders | Yes (toggle on click) |
| Indent guide lines | Yes (via FileNav style) |
| File icons | Yes (MIME-based) |
| Folder icons | Yes (chevron right/down) |
| Selection | Single select only |
| Multi-select | No |
| Drag and drop | No |
| Keyboard navigation | Limited (nav bar default) |
| Lazy loading | Yes (loads one level at a time) |
| Reusable component | No (inline in main.rs, ~100 lines) |

### Limitations

- Not a reusable widget -- hardcoded to file system navigation
- Flat model (segmented_button) with manual position/indent tracking
- No multi-select, no drag-and-drop, no keyboard tree navigation (arrows to expand/collapse)
- Collapse removes entities from model; re-expand rescans filesystem
- No virtual scrolling for large trees

---

## 3. iced (iced-rs/iced)

### Issues

- **Issue #209** (open since 2020): "widgets: spinner, integer/float input, imgui style tree" -- requests an imgui-style collapsible tree. Labels: `feature`, `layout`, `widget`. Maintainer (hecrj) welcomed exploration attempts. No implementation merged.
- **Issue #231** (closed 2020): "Collapsible" widget discussion. Maintainer's position: collapsibles should be composed from existing primitives (Button + Column), not added to core. Iced focuses on "basic primitives providing interactions that cannot be achieved using composition."
- **Issue #553**: RFC on persistent widget tree -- about iced's internal state architecture, not a tree view widget.

### No Tree Widget in Core or iced_aw

- Core iced: No tree widget. `iced::advanced::widget::Tree` is internal state management, not a UI component.
- **iced_aw** (official additional widgets): Badge, Card, ColorPicker, DatePicker, DropDown, NumberInput, SelectionList, Sidebar, TabBar, etc. **No tree widget.**

### iced_file_tree Crate

- Listed on lib.rs and docs.rs as "a lightweight file tree widget for the iced toolkit"
- Version 0.1.0, last updated December 2024
- **No GitHub repository found** -- the crate appears to have been removed or never published properly (crates.io returns "not found")
- Cannot assess maturity, code patterns, or portability
- Status: **Effectively unavailable**

### Maintainer Philosophy

hecrj's position (from issue #231): Iced core provides primitives. Complex composed widgets belong in user-space crates. This means a tree widget would need to be:
1. A separate crate (like iced_aw), or
2. Built into libcosmic as a COSMIC-specific widget

---

## 4. egui Tree Widgets (Design Reference)

Two mature egui tree widget crates provide excellent design patterns, even though they target egui's immediate-mode API rather than iced's retained/Elm architecture.

### egui_ltreeview

- **Repository:** https://github.com/LennysLounge/egui_ltreeview
- **Crate:** https://crates.io/crates/egui_ltreeview
- **Live demo:** https://www.lennyslounge.net/egui_ltreeview/
- **egui version:** 0.33
- **Maturity:** Active, well-documented, has live web demo

**Features:**
- Directory and leaf nodes
- Single and multi-node selection
- Keyboard navigation (arrow keys)
- Drag-and-drop frontend support
- Data-agnostic (builder pattern, not trait-based)

**API Pattern (builder in callback):**
```rust
TreeView::new(id).show(ui, |builder| {
    builder.dir(0, "Root");
    builder.leaf(1, "Ava");
    builder.leaf(2, "Benjamin");
    builder.close_dir();
});
```

**Key Design Insight:** Uses a builder pattern where the user pushes nodes in a callback. The widget handles all rendering, selection, and keyboard navigation internally. The user's data model is completely separate.

### egui-arbor

- **Repository:** https://github.com/kyjohnso/egui-arbor
- **Crate:** https://crates.io/crates/egui-arbor
- **Inspired by:** Blender's outliner
- **Maturity:** Newer, comprehensive architecture doc

**Features:**
- Hierarchical tree view with expand/collapse
- Drag-and-drop with Before/After/Inside positioning
- Multi-selection with keyboard modifiers
- Action icons (visibility/lock/selection toggles)
- Blender-style visibility cascading
- Inline editing (double-click rename)
- Customizable styling

**API Pattern (trait-based):**
```rust
pub trait OutlinerNode {
    type Id: Hash + Eq + Clone;
    fn id(&self) -> Self::Id;
    fn name(&self) -> &str;
    fn is_collection(&self) -> bool;
    fn children(&self) -> Option<&[Self]> where Self: Sized;
    fn children_mut(&mut self) -> Option<&mut Vec<Self>> where Self: Sized;
    fn icon(&self) -> Option<&str> { None }
    fn action_icons(&self) -> Vec<ActionIcon> { /* defaults */ }
}
```

**State management:**
```rust
pub struct OutlinerState {
    expanded: HashSet<egui::Id>,
    editing: Option<egui::Id>,
}
```

**Style system:**
```rust
pub struct Style {
    pub indent: f32,           // 16.0 default
    pub icon_spacing: f32,     // 4.0
    pub row_height: f32,       // 20.0
    pub expand_icon: ExpandIconStyle,
    pub selection_color: Option<Color32>,
    pub hover_color: Option<Color32>,
}
```

**Key Design Insights:**
1. User-owned data -- library does not own the hierarchy
2. Trait-based integration -- works with any data structure
3. State stored separately from data
4. Immediate mode rendering with persistent state

---

## 5. Other Rust GUI Tree Widgets

### tui-tree-widget (ratatui)
- Terminal UI tree widget, not applicable to iced/libcosmic directly
- But has good API design for tree data

### tree_view crate
- Generic tree data structure crate on crates.io, not a GUI widget

---

## 6. Design Recommendations for a libcosmic Tree Widget

### Approach A: Extend segmented_button (Quick Win)

Extract cosmic-edit's pattern into a reusable helper:

**Pros:**
- Works today, no new widget code needed
- Inherits existing COSMIC theming, indent guide lines, keyboard focus
- Already proven in cosmic-edit

**Cons:**
- Flat model -- not truly hierarchical data
- Manual position/indent bookkeeping
- No multi-select, drag-and-drop, or tree-specific keyboard nav
- Poor performance for large trees (no virtualization)

### Approach B: Custom iced Widget (Proper Solution)

Build a dedicated `cosmic::widget::tree_view` as a new widget:

**Recommended API (hybrid of egui patterns adapted for Elm architecture):**

```rust
// Data trait
pub trait TreeNode {
    type Id: Hash + Eq + Clone;
    fn id(&self) -> Self::Id;
    fn label(&self) -> &str;
    fn icon(&self) -> Option<cosmic::widget::icon::Icon> { None }
    fn is_expandable(&self) -> bool;
    fn children(&self) -> &[Self] where Self: Sized;
}

// Widget usage in view()
cosmic::widget::tree_view(&self.tree_data)
    .on_select(Message::TreeNodeSelected)
    .on_expand(Message::TreeNodeExpanded)
    .on_collapse(Message::TreeNodeCollapsed)
    .indent_width(16)
    .row_height(28)
    .into()

// State stored in application model
pub struct TreeViewState<Id> {
    expanded: HashSet<Id>,
    selected: Option<Id>,  // or HashSet<Id> for multi-select
}
```

**Key decisions for iced/libcosmic:**
1. **Elm architecture** -- no mutation in view(), changes via messages
2. **Trait-based data** -- user implements `TreeNode` on their type
3. **Separate state** -- `TreeViewState` lives in application model
4. **COSMIC theming** -- use `cosmic::theme` for colors, spacing, icons
5. **Virtual scrolling** -- essential for large trees (cosmic-edit does not have this)

### Approach C: Composition Widget (Middle Ground)

A "tree_view" that composes existing libcosmic widgets (Column, Button, Row, Container) with proper tree logic:

**Pros:**
- No custom rendering code needed
- Automatic COSMIC theme compliance
- Simpler to implement than a full custom widget

**Cons:**
- Less control over rendering details
- Potentially worse performance than custom widget
- Widget tree depth grows with data tree depth

---

## 7. Summary Table

| Source | Tree Widget? | Maturity | Portability to libcosmic |
|--------|-------------|----------|-------------------------|
| **libcosmic** | No (indent support in segmented_button) | Production | N/A -- already there |
| **cosmic-edit** | Yes (composed, not reusable) | Production | Extract pattern, ~200 lines |
| **iced core** | No | N/A | N/A |
| **iced_aw** | No | N/A | N/A |
| **iced_file_tree** | Claimed, unavailable | Dead/missing | Cannot assess |
| **egui_ltreeview** | Yes (builder pattern) | Active, polished | API design reference only |
| **egui-arbor** | Yes (trait-based) | Active, well-architected | Architecture reference only |

### Bottom Line

The fastest path is to extract cosmic-edit's pattern into a reusable helper function or small crate. The proper long-term solution is a dedicated `cosmic::widget::tree_view` modeled on egui-arbor's trait-based architecture but adapted to iced's Elm/message-passing paradigm. No existing crate can be directly ported -- the fundamental GUI paradigm difference (egui immediate mode vs iced retained/Elm) means any implementation must be written fresh for iced.
