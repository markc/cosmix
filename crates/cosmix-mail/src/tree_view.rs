//! Reusable tree view widget backed by libcosmic's segmented_button with indent.
//!
//! Maintains a logical tree structure and renders it as a flat indented list
//! using `segmented_button::SingleSelectModel` with `FileNav` style and
//! vertical guide lines.
//!
//! # Usage
//!
//! ```rust,ignore
//! let mut tree = TreeView::new();
//!
//! // Build tree from flat pre-order list with depth
//! tree.set_nodes(vec![
//!     TreeNode::new("inbox", "Inbox", "mail-folder-inbox-symbolic", 0, true),
//!     TreeNode::new("important", "Important", "folder-symbolic", 1, false),
//!     TreeNode::new("sent", "Sent", "mail-folder-outbox-symbolic", 0, false),
//! ]);
//!
//! // In view:
//! tree.view(Message::TreeActivated)
//!
//! // In update, on TreeActivated(entity):
//! if let Some(id) = tree.activated(entity) {
//!     if tree.has_children(id) {
//!         tree.toggle(id);
//!     }
//!     // ... load data for id
//! }
//! ```

use cosmic::widget::{icon, segmented_button};
use cosmic::Element;
use std::collections::{HashMap, HashSet};

/// A node in the tree, provided by the application.
#[derive(Clone, Debug)]
pub struct TreeNode {
    /// Unique identifier for this node.
    pub id: String,
    /// Display label.
    pub label: String,
    /// COSMIC icon theme name (e.g., "folder-symbolic").
    pub icon_name: &'static str,
    /// Depth in the tree (0 = root level).
    pub depth: u16,
    /// Whether this node can be expanded (has or may have children).
    pub has_children: bool,
}

impl TreeNode {
    pub fn new(
        id: impl Into<String>,
        label: impl Into<String>,
        icon_name: &'static str,
        depth: u16,
        has_children: bool,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            icon_name,
            depth,
            has_children,
        }
    }
}

/// A tree view widget backed by segmented_button with indent and guide lines.
pub struct TreeView {
    /// All nodes in pre-order traversal (parents before children).
    nodes: Vec<TreeNode>,
    /// Which node IDs are currently expanded.
    expanded: HashSet<String>,
    /// The rendered flat model.
    model: segmented_button::SingleSelectModel,
    /// Map from segmented_button entity to node ID.
    entity_to_id: HashMap<segmented_button::Entity, String>,
    /// Map from node ID to segmented_button entity (for the currently visible items).
    id_to_entity: HashMap<String, segmented_button::Entity>,
}

impl Default for TreeView {
    fn default() -> Self {
        Self::new()
    }
}

impl TreeView {
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            expanded: HashSet::new(),
            model: segmented_button::SingleSelectModel::default(),
            entity_to_id: HashMap::new(),
            id_to_entity: HashMap::new(),
        }
    }

    /// Set the full tree from a flat pre-order list.
    ///
    /// Nodes must be ordered so that children immediately follow their parent,
    /// with `depth = parent.depth + 1`. Call `rebuild()` after to update the view.
    pub fn set_nodes(&mut self, nodes: Vec<TreeNode>) {
        self.nodes = nodes;
        self.rebuild();
    }

    /// Expand all root-level nodes that have children.
    pub fn expand_roots(&mut self) {
        for node in &self.nodes {
            if node.depth == 0 && node.has_children {
                self.expanded.insert(node.id.clone());
            }
        }
        self.rebuild();
    }

    /// Expand a specific node. Call `rebuild()` after if batching changes.
    pub fn expand(&mut self, id: &str) {
        self.expanded.insert(id.to_string());
    }

    /// Collapse a specific node. Call `rebuild()` after if batching changes.
    pub fn collapse(&mut self, id: &str) {
        self.expanded.remove(id);
    }

    /// Toggle expand/collapse for a node and rebuild.
    pub fn toggle(&mut self, id: &str) {
        if self.expanded.contains(id) {
            self.expanded.remove(id);
        } else {
            self.expanded.insert(id.to_string());
        }
        self.rebuild();
    }

    /// Whether a node is currently expanded.
    pub fn is_expanded(&self, id: &str) -> bool {
        self.expanded.contains(id)
    }

    /// Get the node ID for a segmented_button entity (from an activation event).
    pub fn activated(&self, entity: segmented_button::Entity) -> Option<&str> {
        self.entity_to_id.get(&entity).map(|s| s.as_str())
    }

    /// Get the currently active (selected) node ID.
    pub fn active_id(&self) -> Option<&str> {
        let active = self.model.active();
        self.entity_to_id.get(&active).map(|s| s.as_str())
    }

    /// Activate (select) a node by ID.
    pub fn activate(&mut self, id: &str) {
        if let Some(&entity) = self.id_to_entity.get(id) {
            self.model.activate(entity);
        }
    }

    /// Whether a node has children (is expandable).
    pub fn has_children(&self, id: &str) -> bool {
        self.nodes.iter().any(|n| n.id == id && n.has_children)
    }

    /// Get the underlying segmented_button model (for nav_bar integration).
    pub fn model(&self) -> &segmented_button::SingleSelectModel {
        &self.model
    }

    /// Get a mutable reference to the underlying model.
    pub fn model_mut(&mut self) -> &mut segmented_button::SingleSelectModel {
        &mut self.model
    }

    /// Rebuild the flat segmented_button model from the tree + expanded state.
    ///
    /// A node is visible if all its ancestors are expanded. We track this by
    /// maintaining a "visible depth ceiling" — if a collapsed node is at depth N,
    /// all nodes at depth > N are hidden until we see depth <= N again.
    pub fn rebuild(&mut self) {
        // Remember what was active
        let active_id = self.active_id().map(|s| s.to_string());

        self.model = segmented_button::SingleSelectModel::default();
        self.entity_to_id.clear();
        self.id_to_entity.clear();

        // visible_depth: the maximum depth we'll show. Starts at u16::MAX (show all roots).
        // When we encounter a collapsed node at depth D, set visible_depth = D.
        // When we see a node at depth <= visible_depth, reset visible_depth.
        let mut visible_depth: u16 = u16::MAX;

        for node in &self.nodes {
            // If this node's depth is beyond what's visible, skip it
            if node.depth > visible_depth {
                continue;
            }

            // This node is visible. Reset visible_depth ceiling.
            if node.depth <= visible_depth {
                visible_depth = u16::MAX;
            }

            // Build label: prepend expand/collapse indicator for parent nodes
            let label = if node.has_children {
                let arrow = if self.expanded.contains(&node.id) {
                    "\u{25BE}" // ▾ down-pointing triangle
                } else {
                    "\u{25B8}" // ▸ right-pointing triangle
                };
                format!("{arrow}  {}", node.label)
            } else {
                format!("    {}", node.label)
            };

            let entity = self
                .model
                .insert()
                .text(label)
                .indent(node.depth)
                .icon(icon::from_name(node.icon_name))
                .id();

            self.entity_to_id.insert(entity, node.id.clone());
            self.id_to_entity.insert(node.id.clone(), entity);

            // If this node has children but is collapsed, hide everything deeper
            if node.has_children && !self.expanded.contains(&node.id) {
                visible_depth = node.depth;
            }
        }

        // Restore active selection
        if let Some(ref id) = active_id {
            if let Some(&entity) = self.id_to_entity.get(id.as_str()) {
                self.model.activate(entity);
            }
        } else {
            // Activate first item
            self.model.activate_position(0);
        }
    }

    /// Render the tree view as a vertical segmented_button with FileNav style.
    pub fn view<Message: Clone + 'static>(
        &self,
        on_activate: fn(segmented_button::Entity) -> Message,
    ) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();

        widget_view(&self.model, on_activate, spacing.space_m)
    }
}

/// Render a segmented_button model as a vertical FileNav-styled widget.
fn widget_view<'a, Message: Clone + 'static>(
    model: &'a segmented_button::SingleSelectModel,
    on_activate: fn(segmented_button::Entity) -> Message,
    button_height: u16,
) -> Element<'a, Message> {
    segmented_button::vertical(model)
        .style(cosmic::theme::SegmentedButton::FileNav)
        .button_height(button_height.into())
        .indent_spacing(16)
        .on_activate(on_activate)
        .width(cosmic::iced::Length::Fill)
        .into()
}
