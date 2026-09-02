use std::time::Duration;

use cosmix_shell::core::{
    Carousel, CarouselError, ConcealReason, Edge, LogicalSize, MotionError, PanelConfig,
    PanelEffect, PanelInput, PanelMode, PanelMotion, PanelStateMachine, PanelWake, RevealTrigger,
    seed_panel_thickness,
};

fn ms(value: u64) -> Duration {
    Duration::from_millis(value)
}

fn panel() -> PanelStateMachine {
    PanelStateMachine::new(
        PanelConfig::new(100.0, ms(800), ms(200)).unwrap(),
        Duration::ZERO,
    )
    .unwrap()
}

#[test]
fn reveal_is_idempotent() {
    let mut panel = panel();
    panel.apply(ms(0), PanelInput::Reveal).unwrap();
    panel.tick(ms(100)).unwrap();
    let before = panel.snapshot();
    let update = panel.apply(ms(100), PanelInput::Reveal).unwrap();
    assert!(!update.changed);
    assert_eq!(update.snapshot, before);
}

#[test]
fn pointer_leave_conceals_only_after_grace() {
    let mut panel = panel();
    panel.apply(ms(0), PanelInput::Reveal).unwrap();
    panel.tick(ms(200)).unwrap();
    panel.apply(ms(200), PanelInput::PointerEntered).unwrap();
    panel.apply(ms(300), PanelInput::PointerLeft).unwrap();
    assert_eq!(panel.wake(), PanelWake::WakeAt(ms(1_100)));
    panel.tick(ms(1_099)).unwrap();
    assert_eq!(panel.snapshot().mode, PanelMode::Revealed);
    panel.tick(ms(1_100)).unwrap();
    assert_eq!(panel.snapshot().mode, PanelMode::Hidden);
    assert!(panel.snapshot().mapped);
    panel.tick(ms(1_300)).unwrap();
    assert!(!panel.snapshot().mapped);
}

#[test]
fn generic_reveal_has_no_fallback_grace() {
    let mut panel = panel();
    panel.apply(ms(0), PanelInput::Reveal).unwrap();
    assert_eq!(panel.snapshot().hide_at, None);
    panel.tick(ms(1_000)).unwrap();
    assert_eq!(panel.snapshot().mode, PanelMode::Revealed);
}

#[test]
fn corner_and_pointer_are_independent_holds_with_explicit_causes() {
    let mut panel = panel();
    let entered = panel.apply(ms(0), PanelInput::CornerEntered).unwrap();
    assert_eq!(
        entered.effect,
        Some(PanelEffect::Reveal {
            trigger: RevealTrigger::Corner,
        })
    );
    panel.apply(ms(100), PanelInput::PointerEntered).unwrap();
    panel.apply(ms(200), PanelInput::CornerLeft).unwrap();
    assert_eq!(panel.snapshot().hide_at, None);
    panel.apply(ms(300), PanelInput::PointerLeft).unwrap();
    assert_eq!(panel.snapshot().conceal_reason, Some(ConcealReason::Grace));
    panel.apply(ms(400), PanelInput::CornerEntered).unwrap();
    assert_eq!(panel.snapshot().hide_at, None);
    panel.apply(ms(500), PanelInput::PointerLeft).unwrap();
    assert_eq!(panel.snapshot().hide_at, None);
    panel.apply(ms(600), PanelInput::CornerLeft).unwrap();
    assert_eq!(
        panel.snapshot().conceal_reason,
        Some(ConcealReason::CornerLeft)
    );
}

#[test]
fn pin_survives_both_leaves_and_unpin_outside_arms_grace() {
    let mut panel = panel();
    assert_eq!(
        panel.apply(ms(0), PanelInput::Pin).unwrap().effect,
        Some(PanelEffect::Pin { pinned: true })
    );
    panel.apply(ms(1), PanelInput::CornerLeft).unwrap();
    panel.apply(ms(2), PanelInput::PointerLeft).unwrap();
    panel.tick(ms(2_000)).unwrap();
    assert_eq!(panel.snapshot().mode, PanelMode::Pinned);
    assert_eq!(
        panel.apply(ms(2_000), PanelInput::Unpin).unwrap().effect,
        Some(PanelEffect::Pin { pinned: false })
    );
    assert_eq!(panel.snapshot().conceal_reason, Some(ConcealReason::Grace));
}

#[test]
fn pointer_enter_after_corner_left_cancels_corner_conceal() {
    let mut panel = panel();
    panel.apply(ms(0), PanelInput::CornerEntered).unwrap();
    panel.apply(ms(1), PanelInput::CornerLeft).unwrap();
    panel.apply(ms(500), PanelInput::PointerEntered).unwrap();
    panel.tick(ms(2_000)).unwrap();
    assert_eq!(panel.snapshot().mode, PanelMode::Revealed);
    assert_eq!(panel.snapshot().conceal_reason, None);
}

#[test]
fn pointer_leave_during_active_corner_does_not_arm_conceal() {
    let mut panel = panel();
    panel.apply(ms(0), PanelInput::CornerEntered).unwrap();
    panel.apply(ms(100), PanelInput::PointerEntered).unwrap();
    panel.apply(ms(200), PanelInput::PointerLeft).unwrap();

    assert!(panel.snapshot().corner_inside);
    assert!(!panel.snapshot().pointer_inside);
    assert_eq!(panel.snapshot().hide_at, None);
    panel.tick(ms(2_000)).unwrap();
    assert_eq!(panel.snapshot().mode, PanelMode::Revealed);
}

#[test]
fn late_corner_or_pin_effect_supersedes_an_unpresented_expired_conceal() {
    let mut panel = panel();
    panel.apply(ms(0), PanelInput::CornerEntered).unwrap();
    panel.apply(ms(1), PanelInput::CornerLeft).unwrap();
    let entered = panel.apply(ms(1_000), PanelInput::CornerEntered).unwrap();
    assert_eq!(
        entered.effect,
        Some(PanelEffect::Reveal {
            trigger: RevealTrigger::Corner,
        })
    );
    assert_eq!(entered.snapshot.mode, PanelMode::Revealed);

    panel.apply(ms(1_001), PanelInput::CornerLeft).unwrap();
    let pinned = panel.apply(ms(2_000), PanelInput::Pin).unwrap();
    assert_eq!(pinned.effect, Some(PanelEffect::Pin { pinned: true }));
    assert_eq!(pinned.snapshot.mode, PanelMode::Pinned);
}

#[test]
fn pointer_reentry_cancels_grace_deadline() {
    let mut panel = panel();
    panel.apply(ms(0), PanelInput::Reveal).unwrap();
    panel.tick(ms(200)).unwrap();
    panel.apply(ms(210), PanelInput::PointerLeft).unwrap();
    panel.apply(ms(500), PanelInput::PointerEntered).unwrap();
    panel.tick(ms(2_000)).unwrap();
    assert_eq!(panel.snapshot().mode, PanelMode::Revealed);
    assert_eq!(panel.snapshot().hide_at, None);
}

#[test]
fn reveal_reverses_conceal_without_jumping_to_an_endpoint() {
    let mut panel = panel();
    panel.apply(ms(0), PanelInput::Reveal).unwrap();
    panel.tick(ms(100)).unwrap();
    assert_eq!(panel.snapshot().visible_fraction, 0.5);
    panel.apply(ms(100), PanelInput::Hide).unwrap();
    panel.tick(ms(150)).unwrap();
    assert_eq!(panel.snapshot().visible_fraction, 0.25);
    panel.apply(ms(150), PanelInput::Reveal).unwrap();
    assert_eq!(panel.snapshot().visible_fraction, 0.25);
    panel.tick(ms(200)).unwrap();
    assert_eq!(panel.snapshot().visible_fraction, 0.5);
}

#[test]
fn ordinary_hide_never_unpins() {
    let mut panel = panel();
    panel.apply(ms(0), PanelInput::Pin).unwrap();
    panel.tick(ms(200)).unwrap();
    let update = panel.apply(ms(300), PanelInput::Hide).unwrap();
    assert!(!update.changed);
    assert_eq!(update.snapshot.mode, PanelMode::Pinned);
    assert_eq!(update.snapshot.exclusive_zone_px, 100.0);
}

/// `Toggle` mirrors `Hide`: a pinned panel ignores BOTH directions. The law
/// is stated in `PanelInput::Toggle`'s docs and in `apply`'s Toggle arm, and
/// until now was asserted at no level — the toggle rewire's own tests all
/// start from Hidden or Revealed.
#[test]
fn a_pinned_panel_ignores_the_toggle_in_both_directions() {
    let mut panel = panel();
    panel.apply(ms(0), PanelInput::Pin).unwrap();
    panel.tick(ms(200)).unwrap();
    let pinned = panel.snapshot();
    assert_eq!(pinned.mode, PanelMode::Pinned);

    // Pinned is not Hidden, so this takes the "hide otherwise" branch — and
    // the pin must veto it, exactly as `Hide` is vetoed above.
    let update = panel.apply(ms(300), PanelInput::Toggle).unwrap();
    assert!(!update.changed);
    assert_eq!(update.snapshot.mode, PanelMode::Pinned);
    assert_eq!(update.snapshot.exclusive_zone_px, 100.0);

    // And the other direction is no different: a second toggle must not
    // reveal-cycle it either.
    let update = panel.apply(ms(400), PanelInput::Toggle).unwrap();
    assert!(!update.changed);
    assert_eq!(update.snapshot.mode, PanelMode::Pinned);
    assert_eq!(update.snapshot.exclusive_zone_px, 100.0);
}

#[test]
fn escape_never_unpins() {
    let mut panel = panel();
    panel.apply(ms(0), PanelInput::Pin).unwrap();
    panel.tick(ms(200)).unwrap();
    let update = panel.apply(ms(300), PanelInput::Escape).unwrap();
    assert!(!update.changed);
    assert_eq!(update.snapshot.mode, PanelMode::Pinned);
}

#[test]
fn unpin_outside_starts_normal_grace_timer() {
    let mut panel = panel();
    panel.apply(ms(0), PanelInput::Pin).unwrap();
    panel.tick(ms(200)).unwrap();
    panel.apply(ms(250), PanelInput::Unpin).unwrap();
    assert_eq!(panel.snapshot().mode, PanelMode::Revealed);
    assert_eq!(panel.snapshot().hide_at, Some(ms(1_050)));
    assert_eq!(panel.snapshot().exclusive_zone_px, 0.0);
    panel.tick(ms(1_250)).unwrap();
    assert_eq!(panel.snapshot().visible_fraction, 0.0);
    assert!(!panel.snapshot().mapped);
}

#[test]
fn pinning_hidden_panel_maps_it_and_claims_zone_immediately() {
    let mut panel = panel();
    panel.apply(ms(0), PanelInput::Pin).unwrap();
    let snapshot = panel.snapshot();
    assert_eq!(snapshot.mode, PanelMode::Pinned);
    assert!(snapshot.mapped);
    assert_eq!(snapshot.visible_fraction, 0.0);
    assert_eq!(snapshot.exclusive_zone_px, 100.0);
    assert_eq!(panel.wake(), PanelWake::Animate);
}

#[test]
fn pointer_can_reverse_a_partly_concealed_panel() {
    let mut panel = panel();
    panel.apply(ms(0), PanelInput::Reveal).unwrap();
    panel.tick(ms(200)).unwrap();
    panel.apply(ms(200), PanelInput::Hide).unwrap();
    panel.tick(ms(300)).unwrap();
    assert_eq!(panel.snapshot().mode, PanelMode::Hidden);
    assert_eq!(panel.snapshot().visible_fraction, 0.5);
    panel.apply(ms(300), PanelInput::PointerEntered).unwrap();
    assert_eq!(panel.snapshot().mode, PanelMode::Revealed);
    panel.tick(ms(400)).unwrap();
    assert_eq!(panel.snapshot().visible_fraction, 1.0);
}

#[test]
fn panel_time_is_monotonic() {
    let mut panel = panel();
    panel.tick(ms(100)).unwrap();
    assert!(panel.tick(ms(99)).is_err());
    assert!(panel.tick(ms(101)).is_ok());
}

#[test]
fn motion_requires_time_and_reverses_from_current_fraction() {
    assert_eq!(
        PanelMotion::new(Duration::ZERO),
        Err(MotionError::ZeroTravelTime)
    );
    let mut motion = PanelMotion::new(ms(200)).unwrap();
    motion.reveal();
    motion.advance(ms(50));
    assert_eq!(motion.visible_fraction(), 0.25);
    motion.conceal();
    motion.advance(ms(25));
    assert_eq!(motion.visible_fraction(), 0.125);
}

#[test]
fn carousel_wraps_and_selects_stable_ids() {
    let mut carousel = Carousel::new(["nav", "windows", "places"]).unwrap();
    assert_eq!(carousel.active_id(), Some("nav"));
    assert_eq!(carousel.previous_page(), Some("places"));
    assert_eq!(carousel.next_page(), Some("nav"));
    assert!(carousel.select_id("windows"));
    assert_eq!(carousel.active_index(), Some(1));
    assert!(!carousel.select_id("missing"));
    assert!(!carousel.select_index(99));
}

#[test]
fn carousel_rejects_empty_and_duplicate_ids_but_allows_no_pages() {
    assert_eq!(Carousel::empty().active_id(), None);
    assert_eq!(Carousel::new(["", "ok"]), Err(CarouselError::EmptyId));
    assert_eq!(
        Carousel::new(["same", "same"]),
        Err(CarouselError::DuplicateId("same".into()))
    );
}

#[test]
fn e2_thickness_seeds_are_clamped_logical_pixels() {
    let small = LogicalSize::new(800.0, 600.0).unwrap();
    assert_eq!(seed_panel_thickness(Edge::Left, small), 240.0);
    assert_eq!(seed_panel_thickness(Edge::Right, small), 240.0);
    assert_eq!(seed_panel_thickness(Edge::Top, small), 32.0);
    assert_eq!(seed_panel_thickness(Edge::Bottom, small), 60.0);

    let large = LogicalSize::new(8_000.0, 4_000.0).unwrap();
    assert_eq!(seed_panel_thickness(Edge::Left, large), 480.0);
    assert_eq!(seed_panel_thickness(Edge::Top, large), 64.0);
    assert_eq!(seed_panel_thickness(Edge::Bottom, large), 128.0);
}
