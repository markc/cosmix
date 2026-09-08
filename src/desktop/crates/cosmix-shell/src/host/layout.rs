//! Shared logical geometry for hosts that render all panels in one scene.
use crate::core::Edge;
use crate::runtime::ShellFrame;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PanelRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PanelLayout {
    pub panels: [PanelRect; 4],
    pub canvas: PanelRect,
}

/// Top > right > bottom > left, matching the chrome stacking order.
pub fn panel_layout(frame: &ShellFrame) -> PanelLayout {
    let width = frame.geometry.logical_size.width();
    let height = frame.geometry.logical_size.height();
    let pinned = |edge: Edge| frame.panel(edge).exclusive_zone_px;
    let left = pinned(Edge::Left).min(width);
    let right = pinned(Edge::Right).min((width - left).max(0.0));
    let top = pinned(Edge::Top).min(height);
    let bottom = pinned(Edge::Bottom).min((height - top).max(0.0));
    let thickness = |edge: Edge| frame.panel(edge).thickness_px;
    let mut panels = [PanelRect::default(); 4];
    panels[Edge::Top.index()] = PanelRect {
        x: 0.0,
        y: 0.0,
        width,
        height: thickness(Edge::Top),
    };
    panels[Edge::Right.index()] = PanelRect {
        x: (width - thickness(Edge::Right)).max(0.0),
        y: top,
        width: thickness(Edge::Right),
        height: (height - top).max(0.0),
    };
    panels[Edge::Bottom.index()] = PanelRect {
        x: 0.0,
        y: (height - thickness(Edge::Bottom)).max(0.0),
        width: (width - right).max(0.0),
        height: thickness(Edge::Bottom),
    };
    panels[Edge::Left.index()] = PanelRect {
        x: 0.0,
        y: top,
        width: thickness(Edge::Left),
        height: (height - top - bottom).max(0.0),
    };
    PanelLayout {
        panels,
        canvas: PanelRect {
            x: left,
            y: top,
            width: (width - left - right).max(0.0),
            height: (height - top - bottom).max(0.0),
        },
    }
}
