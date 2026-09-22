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

/// Horizontal docks span the output; side docks fit between them.
/// Only docked panels have nonzero exclusive zones. Overlay visibility never
/// changes these reservations or the canvas work area.
pub fn panel_layout(frame: &ShellFrame) -> PanelLayout {
    let width = frame.geometry.logical_size.width();
    let height = frame.geometry.logical_size.height();
    let reserved = |edge: Edge| frame.panel(edge).exclusive_zone_px;
    let left = reserved(Edge::Left).min(width);
    let right = reserved(Edge::Right).min((width - left).max(0.0));
    let top = reserved(Edge::Top).min(height);
    let bottom = reserved(Edge::Bottom).min((height - top).max(0.0));
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
        height: (height - top - bottom).max(0.0),
    };
    panels[Edge::Bottom.index()] = PanelRect {
        x: 0.0,
        y: (height - thickness(Edge::Bottom)).max(0.0),
        width,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{LogicalSize, OutputKey, PanelInput, ShellModel};
    use std::time::Duration;

    fn model() -> ShellModel {
        ShellModel::new(
            OutputKey::new("test").unwrap(),
            LogicalSize::new(1000.0, 800.0).unwrap(),
            Duration::ZERO,
            Duration::from_millis(800),
            Duration::from_millis(200),
        )
        .unwrap()
    }

    #[test]
    fn horizontal_docks_take_precedence_over_side_docks() {
        let mut model = model();
        for edge in [Edge::Top, Edge::Right] {
            model
                .panel_input(edge, Duration::ZERO, PanelInput::Dock)
                .unwrap();
        }
        let frame = ShellFrame::from_model(&model);
        let top = frame.panel(Edge::Top).thickness_px;
        let layout = panel_layout(&frame);
        assert_eq!(layout.panels[Edge::Top.index()].width, 1000.0);
        assert_eq!(layout.panels[Edge::Right.index()].y, top);
        assert_eq!(layout.panels[Edge::Right.index()].height, 800.0 - top);

        for edge in [Edge::Bottom, Edge::Left] {
            model
                .panel_input(edge, Duration::ZERO, PanelInput::Dock)
                .unwrap();
        }
        let frame = ShellFrame::from_model(&model);
        let bottom = frame.panel(Edge::Bottom).thickness_px;
        let layout = panel_layout(&frame);
        assert_eq!(layout.panels[Edge::Top.index()].width, 1000.0);
        assert_eq!(layout.panels[Edge::Bottom.index()].width, 1000.0);
        for edge in [Edge::Left, Edge::Right] {
            let side = layout.panels[edge.index()];
            assert_eq!(side.y, top);
            assert_eq!(side.height, 800.0 - top - bottom);
            assert_eq!(side.y + side.height, layout.panels[Edge::Bottom.index()].y);
        }
        assert_eq!(layout.canvas.height, 800.0 - top - bottom);
    }

    #[test]
    fn side_docks_have_full_height_without_horizontal_docks() {
        let mut model = model();
        for edge in [Edge::Left, Edge::Right] {
            model
                .panel_input(edge, Duration::ZERO, PanelInput::Dock)
                .unwrap();
        }
        let expected = panel_layout(&ShellFrame::from_model(&model));
        for edge in [Edge::Left, Edge::Right] {
            assert_eq!(expected.panels[edge.index()].y, 0.0);
            assert_eq!(expected.panels[edge.index()].height, 800.0);
        }
        model
            .panel_input(Edge::Top, Duration::ZERO, PanelInput::Pin)
            .unwrap();
        model
            .panel_input(Edge::Bottom, Duration::ZERO, PanelInput::Reveal)
            .unwrap();
        assert_eq!(panel_layout(&ShellFrame::from_model(&model)), expected);
    }

    #[test]
    fn bottom_dock_alone_shortens_both_sides() {
        let mut model = model();
        for edge in [Edge::Left, Edge::Right, Edge::Bottom] {
            model
                .panel_input(edge, Duration::ZERO, PanelInput::Dock)
                .unwrap();
        }
        let frame = ShellFrame::from_model(&model);
        let layout = panel_layout(&frame);
        for edge in [Edge::Left, Edge::Right] {
            assert_eq!(layout.panels[edge.index()].y, 0.0);
            assert_eq!(
                layout.panels[edge.index()].height,
                800.0 - frame.panel(Edge::Bottom).thickness_px
            );
        }
        assert_eq!(layout.panels[Edge::Bottom.index()].width, 1000.0);
    }
}
