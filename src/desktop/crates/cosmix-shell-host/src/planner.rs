//! Pure `ShellFrame` to layer-shell request planning.

#![deny(unsafe_code)]

use std::error::Error;
use std::fmt::{Display, Formatter};

use cosmix_shell::core::{Edge, PanelMode};
use cosmix_shell::runtime::{KeyboardInteractivity, PanelPresentation};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OutputGeometry {
    pub width: f32,
    pub height: f32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProtocolLayer {
    Top,
    Overlay,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProtocolAnchor {
    pub top: bool,
    pub right: bool,
    pub bottom: bool,
    pub left: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProtocolMargin {
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub left: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProtocolKeyboardInteractivity {
    None,
    OnDemand,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProtocolOp {
    SetLayer(ProtocolLayer),
    SetAnchor(ProtocolAnchor),
    SetSize {
        width: u32,
        height: u32,
    },
    SetExclusiveZone(i32),
    SetMargin(ProtocolMargin),
    SetKeyboardInteractivity(ProtocolKeyboardInteractivity),
    /// The mapping gate: commit all replayed state without a buffer.
    CommitBufferless,
    /// Commit an atomic property-only update on an already mapped surface.
    Commit,
    /// Remove renderer ownership, drain extraction, then attach NULL and commit.
    Unmap,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct DesiredSurface {
    layer: ProtocolLayer,
    anchor: ProtocolAnchor,
    width: u32,
    height: u32,
    zone: i32,
    margin: ProtocolMargin,
    keyboard: ProtocolKeyboardInteractivity,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PlanError {
    InvalidOutputGeometry(OutputGeometry),
    InvalidVisibleFraction(f32),
    InvalidThickness(f32),
    InvalidExclusiveZone(f32),
}

impl Display for PlanError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidOutputGeometry(geometry) => write!(
                formatter,
                "output geometry must be finite and positive, got {}x{}",
                geometry.width, geometry.height
            ),
            Self::InvalidVisibleFraction(value) => {
                write!(
                    formatter,
                    "visible fraction must be finite in 0..=1, got {value}"
                )
            }
            Self::InvalidThickness(value) => write!(
                formatter,
                "panel thickness must be finite, positive and fit u32, got {value}"
            ),
            Self::InvalidExclusiveZone(value) => write!(
                formatter,
                "exclusive zone must be finite in 0..=i32::MAX, got {value}"
            ),
        }
    }
}

impl Error for PlanError {}

/// Plan the smallest atomic protocol delta. A first map and every remap replay
/// every resettable property before the bufferless configure-gate commit.
pub fn plan_surface(
    previous: Option<&PanelPresentation>,
    next: &PanelPresentation,
    output_geometry: OutputGeometry,
) -> Result<Vec<ProtocolOp>, PlanError> {
    validate_geometry(output_geometry)?;
    validate_panel(next)?;

    if !next.mapped {
        return Ok(if previous.is_some_and(|panel| panel.mapped) {
            vec![ProtocolOp::Unmap]
        } else {
            Vec::new()
        });
    }

    let next_desired = desired(next)?;
    if previous.is_none_or(|panel| !panel.mapped) {
        return Ok(full_replay(next_desired));
    }

    let previous_desired = desired(previous.expect("mapped previous checked above"))?;
    let mut operations = Vec::new();
    if previous_desired.layer != next_desired.layer {
        operations.push(ProtocolOp::SetLayer(next_desired.layer));
    }
    if previous_desired.anchor != next_desired.anchor {
        operations.push(ProtocolOp::SetAnchor(next_desired.anchor));
    }
    if (previous_desired.width, previous_desired.height)
        != (next_desired.width, next_desired.height)
    {
        operations.push(ProtocolOp::SetSize {
            width: next_desired.width,
            height: next_desired.height,
        });
    }
    if previous_desired.zone != next_desired.zone {
        operations.push(ProtocolOp::SetExclusiveZone(next_desired.zone));
    }
    if previous_desired.margin != next_desired.margin {
        operations.push(ProtocolOp::SetMargin(next_desired.margin));
    }
    if previous_desired.keyboard != next_desired.keyboard {
        operations.push(ProtocolOp::SetKeyboardInteractivity(next_desired.keyboard));
    }
    if !operations.is_empty() {
        operations.push(ProtocolOp::Commit);
    }
    Ok(operations)
}

fn validate_geometry(geometry: OutputGeometry) -> Result<(), PlanError> {
    if !geometry.width.is_finite()
        || !geometry.height.is_finite()
        || geometry.width <= 0.0
        || geometry.height <= 0.0
    {
        return Err(PlanError::InvalidOutputGeometry(geometry));
    }
    Ok(())
}

fn validate_panel(panel: &PanelPresentation) -> Result<(), PlanError> {
    if !panel.visible_fraction.is_finite() || !(0.0..=1.0).contains(&panel.visible_fraction) {
        return Err(PlanError::InvalidVisibleFraction(panel.visible_fraction));
    }
    if !panel.thickness_px.is_finite()
        || panel.thickness_px <= 0.0
        || f64::from(panel.thickness_px).round() > f64::from(i32::MAX)
        || panel.thickness_px.round() < 1.0
    {
        return Err(PlanError::InvalidThickness(panel.thickness_px));
    }
    if !panel.exclusive_zone_px.is_finite()
        || panel.exclusive_zone_px < 0.0
        || f64::from(panel.exclusive_zone_px) > f64::from(i32::MAX)
    {
        return Err(PlanError::InvalidExclusiveZone(panel.exclusive_zone_px));
    }
    Ok(())
}

fn desired(panel: &PanelPresentation) -> Result<DesiredSurface, PlanError> {
    validate_panel(panel)?;
    let thickness = panel.thickness_px.round() as u32;
    let (anchor, width, height) = match panel.edge {
        Edge::Left => (
            ProtocolAnchor {
                top: true,
                right: false,
                bottom: true,
                left: true,
            },
            thickness,
            0,
        ),
        Edge::Right => (
            ProtocolAnchor {
                top: true,
                right: true,
                bottom: true,
                left: false,
            },
            thickness,
            0,
        ),
        Edge::Top => (
            ProtocolAnchor {
                top: true,
                right: true,
                bottom: false,
                left: true,
            },
            0,
            thickness,
        ),
        Edge::Bottom => (
            ProtocolAnchor {
                top: false,
                right: true,
                bottom: true,
                left: true,
            },
            0,
            thickness,
        ),
    };
    let pinned = panel.mode == PanelMode::Pinned;
    let hidden = if pinned {
        0
    } else {
        -((1.0 - panel.visible_fraction) * panel.thickness_px).round() as i32
    };
    let margin = match panel.edge {
        Edge::Left => ProtocolMargin {
            left: hidden,
            ..ProtocolMargin::default()
        },
        Edge::Right => ProtocolMargin {
            right: hidden,
            ..ProtocolMargin::default()
        },
        Edge::Top => ProtocolMargin {
            top: hidden,
            ..ProtocolMargin::default()
        },
        Edge::Bottom => ProtocolMargin {
            bottom: hidden,
            ..ProtocolMargin::default()
        },
    };
    Ok(DesiredSurface {
        layer: if pinned {
            ProtocolLayer::Top
        } else {
            ProtocolLayer::Overlay
        },
        anchor,
        width,
        height,
        zone: if pinned { thickness as i32 } else { 0 },
        margin,
        keyboard: match panel.keyboard_interactivity {
            KeyboardInteractivity::None => ProtocolKeyboardInteractivity::None,
            KeyboardInteractivity::OnDemand => ProtocolKeyboardInteractivity::OnDemand,
        },
    })
}

fn full_replay(surface: DesiredSurface) -> Vec<ProtocolOp> {
    vec![
        ProtocolOp::SetLayer(surface.layer),
        ProtocolOp::SetAnchor(surface.anchor),
        ProtocolOp::SetSize {
            width: surface.width,
            height: surface.height,
        },
        ProtocolOp::SetExclusiveZone(surface.zone),
        ProtocolOp::SetMargin(surface.margin),
        ProtocolOp::SetKeyboardInteractivity(surface.keyboard),
        ProtocolOp::CommitBufferless,
    ]
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use pretty_assertions::assert_eq;

    use super::*;

    const GEOMETRY: OutputGeometry = OutputGeometry {
        width: 1_920.0,
        height: 1_080.0,
    };

    fn panel(edge: Edge, mode: PanelMode, mapped: bool, fraction: f32) -> PanelPresentation {
        PanelPresentation {
            edge,
            mode,
            mapped,
            visible_fraction: fraction,
            thickness_px: 101.0,
            exclusive_zone_px: if mode == PanelMode::Pinned {
                101.0
            } else {
                0.0
            },
            keyboard_interactivity: if mapped {
                KeyboardInteractivity::OnDemand
            } else {
                KeyboardInteractivity::None
            },
            page_ids: Arc::default(),
            active_page_id: None,
        }
    }

    fn replay(edge: Edge) -> Vec<ProtocolOp> {
        plan_surface(None, &panel(edge, PanelMode::Revealed, true, 1.0), GEOMETRY).unwrap()
    }

    #[test]
    fn anchor_and_stretched_size_table_covers_all_edges() {
        let cases = [
            (
                Edge::Left,
                ProtocolAnchor {
                    top: true,
                    right: false,
                    bottom: true,
                    left: true,
                },
                (101, 0),
            ),
            (
                Edge::Right,
                ProtocolAnchor {
                    top: true,
                    right: true,
                    bottom: true,
                    left: false,
                },
                (101, 0),
            ),
            (
                Edge::Top,
                ProtocolAnchor {
                    top: true,
                    right: true,
                    bottom: false,
                    left: true,
                },
                (0, 101),
            ),
            (
                Edge::Bottom,
                ProtocolAnchor {
                    top: false,
                    right: true,
                    bottom: true,
                    left: true,
                },
                (0, 101),
            ),
        ];
        for (edge, anchor, size) in cases {
            let operations = replay(edge);
            assert_eq!(operations[1], ProtocolOp::SetAnchor(anchor));
            assert_eq!(
                operations[2],
                ProtocolOp::SetSize {
                    width: size.0,
                    height: size.1,
                }
            );
        }
    }

    #[test]
    fn non_pinned_fraction_table_uses_edge_margin_and_half_away_rounding() {
        for (fraction, expected) in [(0.0, -101), (0.5, -51), (1.0, 0)] {
            let operations = plan_surface(
                None,
                &panel(Edge::Right, PanelMode::Revealed, true, fraction),
                GEOMETRY,
            )
            .unwrap();
            assert_eq!(
                operations[4],
                ProtocolOp::SetMargin(ProtocolMargin {
                    right: expected,
                    ..ProtocolMargin::default()
                })
            );
        }
    }

    #[test]
    fn dynamic_pinning_flips_overlay_to_top_and_claims_full_zone() {
        let revealed = panel(Edge::Top, PanelMode::Revealed, true, 0.5);
        let pinned = panel(Edge::Top, PanelMode::Pinned, true, 0.5);
        assert_eq!(
            plan_surface(Some(&revealed), &pinned, GEOMETRY).unwrap(),
            vec![
                ProtocolOp::SetLayer(ProtocolLayer::Top),
                ProtocolOp::SetExclusiveZone(101),
                ProtocolOp::SetMargin(ProtocolMargin::default()),
                ProtocolOp::Commit,
            ]
        );
        assert_eq!(
            plan_surface(Some(&pinned), &revealed, GEOMETRY).unwrap(),
            vec![
                ProtocolOp::SetLayer(ProtocolLayer::Overlay),
                ProtocolOp::SetExclusiveZone(0),
                ProtocolOp::SetMargin(ProtocolMargin {
                    top: -51,
                    ..ProtocolMargin::default()
                }),
                ProtocolOp::Commit,
            ]
        );
    }

    #[test]
    fn pin_from_hidden_maps_top_with_full_zone_and_zero_protocol_margin() {
        let hidden = panel(Edge::Left, PanelMode::Hidden, false, 0.0);
        let pinned = panel(Edge::Left, PanelMode::Pinned, true, 0.0);
        let operations = plan_surface(Some(&hidden), &pinned, GEOMETRY).unwrap();
        assert_eq!(operations[0], ProtocolOp::SetLayer(ProtocolLayer::Top));
        assert_eq!(operations[3], ProtocolOp::SetExclusiveZone(101));
        assert_eq!(
            operations[4],
            ProtocolOp::SetMargin(ProtocolMargin::default())
        );
        assert_eq!(operations.last(), Some(&ProtocolOp::CommitBufferless));
    }

    #[test]
    fn mapped_hidden_is_conceal_motion_and_final_hidden_unmaps() {
        let revealed = panel(Edge::Bottom, PanelMode::Revealed, true, 1.0);
        let concealing = panel(Edge::Bottom, PanelMode::Hidden, true, 0.5);
        let hidden = panel(Edge::Bottom, PanelMode::Hidden, false, 0.0);
        assert_eq!(
            plan_surface(Some(&revealed), &concealing, GEOMETRY).unwrap(),
            vec![
                ProtocolOp::SetMargin(ProtocolMargin {
                    bottom: -51,
                    ..ProtocolMargin::default()
                }),
                ProtocolOp::Commit,
            ]
        );
        assert_eq!(
            plan_surface(Some(&concealing), &hidden, GEOMETRY).unwrap(),
            vec![ProtocolOp::Unmap]
        );
    }

    #[test]
    fn no_protocol_change_is_a_no_op_even_if_content_changes() {
        let previous = panel(Edge::Left, PanelMode::Revealed, true, 1.0);
        let mut next = previous.clone();
        next.active_page_id = Some("other".to_owned());
        assert!(
            plan_surface(Some(&previous), &next, GEOMETRY)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn remap_replays_every_property_and_configure_gate() {
        let hidden = panel(Edge::Right, PanelMode::Hidden, false, 0.0);
        let remapped = panel(Edge::Right, PanelMode::Revealed, true, 0.5);
        assert_eq!(
            plan_surface(Some(&hidden), &remapped, GEOMETRY).unwrap(),
            plan_surface(None, &remapped, GEOMETRY).unwrap()
        );
    }

    #[test]
    fn rejects_non_finite_values_and_invalid_zone_range() {
        for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut invalid = panel(Edge::Left, PanelMode::Revealed, true, 1.0);
            invalid.visible_fraction = value;
            assert!(matches!(
                plan_surface(None, &invalid, GEOMETRY),
                Err(PlanError::InvalidVisibleFraction(_))
            ));
        }
        let mut invalid = panel(Edge::Left, PanelMode::Pinned, true, 1.0);
        invalid.exclusive_zone_px = -1.0;
        assert_eq!(
            plan_surface(None, &invalid, GEOMETRY),
            Err(PlanError::InvalidExclusiveZone(-1.0))
        );
        invalid.exclusive_zone_px = f32::INFINITY;
        assert!(matches!(
            plan_surface(None, &invalid, GEOMETRY),
            Err(PlanError::InvalidExclusiveZone(_))
        ));
        invalid.exclusive_zone_px = 101.0;
        invalid.thickness_px = f32::NAN;
        assert!(matches!(
            plan_surface(None, &invalid, GEOMETRY),
            Err(PlanError::InvalidThickness(_))
        ));
        invalid.thickness_px = i32::MAX as f32;
        assert!(matches!(
            plan_surface(None, &invalid, GEOMETRY),
            Err(PlanError::InvalidThickness(_))
        ));
        assert!(matches!(
            plan_surface(
                None,
                &panel(Edge::Left, PanelMode::Revealed, true, 1.0),
                OutputGeometry {
                    width: f32::NAN,
                    height: 1.0,
                },
            ),
            Err(PlanError::InvalidOutputGeometry(_))
        ));
    }

    #[test]
    fn keyboard_mapping_is_strictly_none_or_on_demand() {
        let hidden = panel(Edge::Top, PanelMode::Hidden, false, 0.0);
        let mapped = panel(Edge::Top, PanelMode::Revealed, true, 1.0);
        assert_eq!(
            plan_surface(None, &mapped, GEOMETRY).unwrap()[5],
            ProtocolOp::SetKeyboardInteractivity(ProtocolKeyboardInteractivity::OnDemand)
        );
        let mut no_keyboard = mapped.clone();
        no_keyboard.keyboard_interactivity = KeyboardInteractivity::None;
        assert_eq!(
            plan_surface(None, &no_keyboard, GEOMETRY).unwrap()[5],
            ProtocolOp::SetKeyboardInteractivity(ProtocolKeyboardInteractivity::None)
        );
        assert!(plan_surface(None, &hidden, GEOMETRY).unwrap().is_empty());
    }
}
