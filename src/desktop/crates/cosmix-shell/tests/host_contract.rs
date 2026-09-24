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

fn carousel_after_removal() -> ShellModel {
    let mut model = model();
    model
        .declare_carousel(Edge::Left, ["alpha", "beta", "gamma"])
        .unwrap();
    for name in ["alpha", "beta", "gamma"] {
        model.carousel_mut(Edge::Left).register(name).unwrap();
    }
    model
        .panel_input(Edge::Left, ms(0), PanelInput::Reveal)
        .unwrap();
    let carousel = model.carousel_mut(Edge::Left);
    carousel.activate("gamma").unwrap();
    carousel.remove("gamma").unwrap();
    assert_eq!(carousel.active_id(), Some("beta"));
    assert_eq!(carousel.last_selected(), Some("alpha"));
    model
}

#[test]
fn carousel_corner_reveal_restores_memory_after_removal() {
    let mut model = carousel_after_removal();
    model.tick(ms(200)).unwrap();
    model
        .panel_input(Edge::Left, ms(200), PanelInput::Hide)
        .unwrap();
    assert_eq!(model.carousel(Edge::Left).active_id(), Some("beta"));
    model.tick(ms(400)).unwrap();
    assert!(!model.panel(Edge::Left).mapped);
    model
        .corner_event(
            ms(400),
            CornerEvent::Entered {
                corner: Corner::TopLeft,
                dwell: ms(200),
                trigger: CornerTrigger::Dwell,
            },
        )
        .unwrap();
    assert!(model.panel(Edge::Left).transient_revealed);
    assert_eq!(model.carousel(Edge::Left).active_id(), Some("alpha"));
    assert_eq!(
        ShellFrame::from_model(&model)
            .panel(Edge::Left)
            .active_page_id
            .as_deref(),
        Some("alpha")
    );
}

#[test]
fn carousel_transient_reveal_restores_memory_only_from_hidden() {
    for input in [
        PanelInput::Reveal,
        PanelInput::Toggle,
        PanelInput::CornerEntered,
    ] {
        let mut model = carousel_after_removal();
        // A repeated reveal while already visible must retain removal landing.
        model
            .panel_input(Edge::Left, ms(0), PanelInput::Reveal)
            .unwrap();
        assert_eq!(model.carousel(Edge::Left).active_id(), Some("beta"));
        model.tick(ms(200)).unwrap();
        model
            .panel_input(Edge::Left, ms(200), PanelInput::Hide)
            .unwrap();
        // Still mapped during concealment, but logically hidden.
        assert!(model.panel(Edge::Left).mapped);
        model.panel_input(Edge::Left, ms(200), input).unwrap();
        assert_eq!(model.carousel(Edge::Left).active_id(), Some("alpha"));
    }
}

#[test]
fn carousel_reveal_after_expired_grace_restores_memory_without_prior_tick() {
    for input in [
        PanelInput::Reveal,
        PanelInput::Toggle,
        PanelInput::CornerEntered,
    ] {
        let mut model = carousel_after_removal();
        model
            .panel_input(Edge::Left, ms(0), PanelInput::CornerEntered)
            .unwrap();
        model
            .panel_input(Edge::Left, ms(1), PanelInput::CornerLeft)
            .unwrap();
        assert_eq!(model.carousel(Edge::Left).active_id(), Some("beta"));
        model.panel_input(Edge::Left, ms(1000), input).unwrap();
        assert!(model.panel(Edge::Left).transient_revealed);
        assert_eq!(model.carousel(Edge::Left).active_id(), Some("alpha"));
    }
}

#[test]
fn carousel_persistent_reveal_preserves_removal_landing() {
    for mode in [PanelMode::Pinned, PanelMode::Docked] {
        let mut model = carousel_after_removal();
        model.set_mode(Edge::Left, ms(0), mode).unwrap();
        model
            .panel_input(Edge::Left, ms(0), PanelInput::Hide)
            .unwrap();
        model
            .panel_input(Edge::Left, ms(0), PanelInput::Reveal)
            .unwrap();
        assert_eq!(model.carousel(Edge::Left).active_id(), Some("beta"));
    }
}

#[test]
fn carousel_redeclare_promotes_tail_and_preserves_remaining_tail_order() {
    let mut model = model();
    model.declare_carousel(Edge::Left, ["alpha"]).unwrap();
    for name in ["alpha", "tail-one", "beta", "tail-two"] {
        model.carousel_mut(Edge::Left).register(name).unwrap();
    }
    model.carousel_mut(Edge::Left).activate("beta").unwrap();
    model
        .declare_carousel(Edge::Left, ["beta", "alpha"])
        .unwrap();
    let carousel = model.carousel(Edge::Left);
    assert_eq!(
        carousel.page_ids(),
        ["beta", "alpha", "tail-one", "tail-two"]
    );
    assert_eq!(carousel.active_id(), Some("beta"));
    assert_eq!(carousel.active_index(), Some(0));
    assert_eq!(carousel.last_selected(), Some("beta"));
    let carousel = model.carousel_mut(Edge::Left);
    carousel.activate("beta").unwrap();
    carousel.remove("beta").unwrap();
    carousel.register("beta").unwrap();
    assert_eq!(
        carousel.page_ids(),
        ["beta", "alpha", "tail-one", "tail-two"]
    );
}

#[test]
fn carousel_redeclare_demotes_live_names_and_discards_obsolete_empty_slots() {
    let mut model = model();
    model
        .declare_carousel(Edge::Left, ["alpha", "beta", "empty"])
        .unwrap();
    for name in ["alpha", "beta", "tail-one", "tail-two"] {
        model.carousel_mut(Edge::Left).register(name).unwrap();
    }
    model.carousel_mut(Edge::Left).activate("alpha").unwrap();
    model.declare_carousel(Edge::Left, ["beta"]).unwrap();
    let carousel = model.carousel(Edge::Left);
    assert_eq!(
        carousel.page_ids(),
        ["beta", "alpha", "tail-one", "tail-two"]
    );
    assert_eq!(carousel.active_id(), Some("alpha"));
    assert_eq!(carousel.last_selected(), Some("alpha"));
    let carousel = model.carousel_mut(Edge::Left);
    carousel.register("empty").unwrap();
    carousel.remove("alpha").unwrap();
    carousel.register("alpha").unwrap();
    assert_eq!(
        carousel.page_ids(),
        ["beta", "tail-one", "tail-two", "empty", "alpha"]
    );
}

#[test]
fn carousel_redeclare_reorders_by_name_preserving_distinct_selection_and_memory() {
    let mut model = carousel_after_removal();
    model
        .declare_carousel(Edge::Left, ["beta", "new", "alpha"])
        .unwrap();
    let carousel = model.carousel(Edge::Left);
    assert_eq!(carousel.page_ids(), ["beta", "alpha"]);
    assert_eq!(carousel.active_id(), Some("beta"));
    assert_eq!(carousel.active_index(), Some(0));
    assert_eq!(carousel.last_selected(), Some("alpha"));
    model.carousel_mut(Edge::Left).register("new").unwrap();
    assert_eq!(
        model.carousel(Edge::Left).page_ids(),
        ["beta", "new", "alpha"]
    );
    model
        .panel_input(Edge::Left, ms(0), PanelInput::Hide)
        .unwrap();
    model
        .panel_input(Edge::Left, ms(0), PanelInput::Reveal)
        .unwrap();
    assert_eq!(model.carousel(Edge::Left).active_id(), Some("alpha"));
}

#[test]
fn carousel_invalid_redeclaration_leaves_registry_unchanged() {
    let mut model = carousel_after_removal();
    let before = model.carousel(Edge::Left).clone();
    for names in [["alpha", "alpha"], ["alpha", ""]] {
        assert!(model.declare_carousel(Edge::Left, names).is_err());
        assert_eq!(model.carousel(Edge::Left), &before);
    }
}

#[test]
fn carousel_default_reveal_uses_primary_and_skips_empty_slots() {
    let mut model = model();
    model
        .declare_carousel(Edge::Left, ["alpha", "beta", "gamma"])
        .unwrap();
    model.carousel_mut(Edge::Left).register("gamma").unwrap();
    model.carousel_mut(Edge::Left).register("beta").unwrap();
    assert_eq!(model.carousel(Edge::Left).active_id(), Some("gamma"));
    model
        .panel_input(Edge::Left, ms(0), PanelInput::Reveal)
        .unwrap();
    assert_eq!(model.carousel(Edge::Left).active_id(), Some("beta"));
    model.carousel_mut(Edge::Left).register("alpha").unwrap();
    assert_eq!(model.carousel(Edge::Left).active_id(), Some("beta"));
    model
        .panel_input(Edge::Left, ms(0), PanelInput::Hide)
        .unwrap();
    model
        .panel_input(Edge::Left, ms(0), PanelInput::Reveal)
        .unwrap();
    assert_eq!(model.carousel(Edge::Left).active_id(), Some("alpha"));
    assert_eq!(model.carousel(Edge::Left).last_selected(), None);
}

#[test]
fn carousel_intro_restores_memory_only_from_hidden() {
    let mut model = carousel_after_removal();
    model.start_intro(ms(100));
    assert_eq!(model.carousel(Edge::Left).active_id(), Some("beta"));
    model
        .panel_input(Edge::Left, ms(0), PanelInput::Hide)
        .unwrap();
    model.start_intro(ms(100));
    assert_eq!(model.carousel(Edge::Left).active_id(), Some("alpha"));
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
    assert_eq!(right.mode, PanelMode::Hidden);
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
        .panel_input(Edge::Left, Duration::ZERO, PanelInput::Dock)
        .unwrap();
    model.tick(ms(200)).unwrap();
    model
        .panel_input(Edge::Left, ms(300), PanelInput::Hide)
        .unwrap();
    let frame = ShellFrame::from_model(&model);
    let left = frame.panel(Edge::Left);
    assert_eq!(left.mode, PanelMode::Docked);
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
    let frame = ShellFrame::from_model(&model);
    assert_eq!(frame.wake, WakePolicy::Animate);
    assert_eq!(
        frame.wake_deadline,
        Some(ms(810)),
        "animation must not mask the grace timer"
    );
    model.tick(ms(809)).unwrap();
    assert_eq!(model.panel(Edge::Top).mode, PanelMode::Hidden);
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
            assert_eq!(model.panel(edge).mode, PanelMode::Hidden);
            assert_eq!(model.panel(edge).transient_revealed, edge == expected_edge);
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
