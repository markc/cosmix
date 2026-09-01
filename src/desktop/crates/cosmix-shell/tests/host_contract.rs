use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use cosmix_shell::core::{
    Carousel, ConcealReason, Corner, CornerEvent, CornerTrigger, Edge, LogicalSize, OutputKey,
    PanelEffect, PanelInput, PanelMode, RevealTrigger, ShellModel,
};
use cosmix_shell::host::ShellHost;
use cosmix_shell::runtime::{HostGeometry, KeyboardInteractivity, ShellFrame, WakePolicy};

fn ms(value: u64) -> Duration {
    Duration::from_millis(value)
}

fn model() -> ShellModel {
    ShellModel::new(
        OutputKey::new("dev-output").unwrap(),
        LogicalSize::new(1_600.0, 1_000.0).unwrap(),
        Duration::ZERO,
        ms(800),
        ms(200),
    )
    .unwrap()
}

#[derive(Debug)]
struct MockHost {
    geometry: HostGeometry,
    mounts: [u8; 4],
    frames: Vec<ShellFrame>,
    wake_policies: Vec<WakePolicy>,
}

impl MockHost {
    fn reconcile(&mut self, frame: &ShellFrame) -> Result<(), Infallible> {
        self.apply(frame)?;
        self.set_wake_policy(frame.wake)
    }
}

impl ShellHost for MockHost {
    type Error = Infallible;
    type Mount = u8;

    fn geometry(&self) -> &HostGeometry {
        &self.geometry
    }

    fn panel_mount(&self, edge: Edge) -> Self::Mount {
        self.mounts[edge.index()]
    }

    fn apply(&mut self, frame: &ShellFrame) -> Result<(), Self::Error> {
        self.frames.push(frame.clone());
        Ok(())
    }

    fn set_wake_policy(&mut self, policy: WakePolicy) -> Result<(), Self::Error> {
        self.wake_policies.push(policy);
        Ok(())
    }
}

fn host(model: &ShellModel) -> MockHost {
    MockHost {
        geometry: HostGeometry {
            output: model.output().clone(),
            logical_size: model.geometry(),
        },
        mounts: [10, 11, 12, 13],
        frames: Vec::new(),
        wake_policies: Vec::new(),
    }
}

#[test]
fn host_exposes_geometry_and_one_stable_mount_per_edge() {
    let model = model();
    let host = host(&model);
    assert_eq!(host.geometry().output.as_str(), "dev-output");
    assert_eq!(host.geometry().logical_size, model.geometry());
    let mounts = Edge::ALL.map(|edge| host.panel_mount(edge));
    assert_eq!(mounts, [10, 11, 12, 13]);
}

#[test]
fn corner_reveal_reconciles_an_animating_on_demand_panel() {
    let mut model = model();
    let mut host = host(&model);
    model
        .corner_event(
            Duration::ZERO,
            CornerEvent::Entered {
                corner: Corner::BottomRight,
                dwell: ms(200),
                trigger: CornerTrigger::Dwell,
            },
        )
        .unwrap();
    let frame = ShellFrame::from_model(&model);
    let right = frame.panel(Edge::Right);
    assert_eq!(right.mode, PanelMode::Revealed);
    assert!(right.mapped);
    assert_eq!(right.visible_fraction, 0.0);
    assert_eq!(right.exclusive_zone_px, 0.0);
    assert_eq!(
        right.keyboard_interactivity,
        KeyboardInteractivity::OnDemand
    );
    assert_eq!(frame.wake, WakePolicy::Animate);
    host.reconcile(&frame).unwrap();
    assert_eq!(host.frames, vec![frame]);
    assert_eq!(host.wake_policies, vec![WakePolicy::Animate]);
}

#[test]
fn settled_corner_reveal_waits_for_left_then_returns_idle_after_conceal() {
    let mut model = model();
    model
        .panel_input(Edge::Bottom, Duration::ZERO, PanelInput::CornerEntered)
        .unwrap();
    model.tick(ms(200)).unwrap();
    let revealed = ShellFrame::from_model(&model);
    assert_eq!(revealed.panel(Edge::Bottom).visible_fraction, 1.0);
    assert_eq!(revealed.wake, WakePolicy::Idle);
    model
        .panel_input(Edge::Bottom, ms(200), PanelInput::CornerLeft)
        .unwrap();
    assert_eq!(
        ShellFrame::from_model(&model).wake,
        WakePolicy::WakeAt(ms(1_000))
    );
    model.tick(ms(1_000)).unwrap();
    assert_eq!(ShellFrame::from_model(&model).wake, WakePolicy::Animate);
    model.tick(ms(1_200)).unwrap();
    assert_eq!(ShellFrame::from_model(&model).wake, WakePolicy::Idle);
}

#[test]
fn pinned_frame_claims_zone_and_ordinary_hide_cannot_release_it() {
    let mut model = model();
    model.set_carousel(Edge::Left, Carousel::new(["nav", "places"]).unwrap());
    model
        .panel_input(Edge::Left, Duration::ZERO, PanelInput::Pin)
        .unwrap();
    model.tick(ms(200)).unwrap();
    model
        .panel_input(Edge::Left, ms(300), PanelInput::Hide)
        .unwrap();
    let frame = ShellFrame::from_model(&model);
    let left = frame.panel(Edge::Left);
    assert_eq!(left.mode, PanelMode::Pinned);
    assert_eq!(left.exclusive_zone_px, left.thickness_px);
    assert_eq!(left.active_page_id.as_deref(), Some("nav"));
    assert_eq!(frame.wake, WakePolicy::Idle);
}

#[test]
fn frames_share_carousel_page_schema_without_cloning_ids() {
    let mut model = model();
    model.set_carousel(Edge::Left, Carousel::new(["nav", "places"]).unwrap());
    let schema = model.carousel(Edge::Left).shared_page_ids();

    let first = ShellFrame::from_model(&model);
    let second = ShellFrame::from_model(&model);

    assert!(Arc::ptr_eq(&schema, &first.panel(Edge::Left).page_ids));
    assert!(Arc::ptr_eq(
        &first.panel(Edge::Left).page_ids,
        &second.panel(Edge::Left).page_ids,
    ));
}

#[test]
fn corner_left_arms_attributed_grace_and_conceals_at_deadline() {
    let mut model = model();
    model
        .corner_event(
            Duration::ZERO,
            CornerEvent::Entered {
                corner: Corner::TopRight,
                dwell: ms(200),
                trigger: CornerTrigger::Compositor,
            },
        )
        .unwrap();
    let left = model
        .corner_event(
            ms(10),
            CornerEvent::Left {
                corner: Corner::TopRight,
            },
        )
        .unwrap()
        .unwrap();
    assert_eq!(left.snapshot.hide_at, Some(ms(810)));
    assert_eq!(
        left.snapshot.conceal_reason,
        Some(ConcealReason::CornerLeft)
    );
    model.tick(ms(809)).unwrap();
    assert_eq!(model.panel(Edge::Top).mode, PanelMode::Revealed);
    let concealed = model.tick(ms(810)).unwrap();
    assert_eq!(
        concealed[Edge::Top.index()].effect,
        Some(PanelEffect::Conceal {
            reason: ConcealReason::CornerLeft,
        })
    );
}

#[test]
fn corner_dwell_is_diagnostic_only_and_duplicate_transitions_are_idempotent() {
    let mut model = model();
    let entered = CornerEvent::Entered {
        corner: Corner::TopLeft,
        dwell: ms(5_000),
        trigger: CornerTrigger::Compositor,
    };
    assert_eq!(
        model
            .corner_event(Duration::ZERO, entered)
            .unwrap()
            .unwrap()
            .effect,
        Some(PanelEffect::Reveal {
            trigger: RevealTrigger::Corner,
        })
    );
    assert_eq!(
        model
            .corner_event(Duration::ZERO, entered)
            .unwrap()
            .unwrap()
            .effect,
        None
    );
    let left = CornerEvent::Left {
        corner: Corner::TopLeft,
    };
    model.corner_event(ms(1), left).unwrap();
    let deadline = model.panel(Edge::Left).hide_at;
    model.corner_event(ms(2), left).unwrap();
    assert_eq!(model.panel(Edge::Left).hide_at, deadline);
}

#[test]
fn clockwise_corner_mapping_reaches_each_edge_once() {
    let mappings = [
        (Corner::TopLeft, Edge::Left),
        (Corner::BottomLeft, Edge::Bottom),
        (Corner::BottomRight, Edge::Right),
        (Corner::TopRight, Edge::Top),
    ];
    for (corner, expected_edge) in mappings {
        let mut model = model();
        model
            .corner_event(
                Duration::ZERO,
                CornerEvent::Entered {
                    corner,
                    dwell: ms(200),
                    trigger: CornerTrigger::Dwell,
                },
            )
            .unwrap();
        for edge in Edge::ALL {
            let expected = if edge == expected_edge {
                PanelMode::Revealed
            } else {
                PanelMode::Hidden
            };
            assert_eq!(model.panel(edge).mode, expected);
        }
    }
}

#[test]
fn shell_rejects_cross_panel_time_regression_before_partial_tick() {
    let mut model = model();
    model
        .panel_input(Edge::Right, ms(100), PanelInput::Reveal)
        .unwrap();
    assert!(model.tick(ms(99)).is_err());
    assert_eq!(model.panel(Edge::Left).visible_fraction, 0.0);
    assert_eq!(model.panel(Edge::Right).visible_fraction, 0.0);
    model.tick(ms(200)).unwrap();
    assert_eq!(model.panel(Edge::Right).visible_fraction, 0.5);
}
