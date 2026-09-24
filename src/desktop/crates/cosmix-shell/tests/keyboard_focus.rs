use std::time::Duration;

use cosmix_shell::core::{
    Edge, FOCUS_GRANT_TIMEOUT, FocusDirective, FocusStop, LogicalSize, OutputKey, PanelInput,
    PanelMode, ShellModel,
};
use cosmix_shell::runtime::{KeyboardInteractivity, ShellFrame};

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

fn keyboard(model: &ShellModel, edge: Edge) -> KeyboardInteractivity {
    ShellFrame::from_model(model)
        .panel(edge)
        .keyboard_interactivity
}

#[test]
fn cycle_focus_walks_visible_panels_then_apps() {
    use KeyboardInteractivity::{Exclusive, None as Refuse, OnDemand};
    let mut model = model();
    model.set_mode(Edge::Left, ms(0), PanelMode::Pinned).unwrap();
    model.set_mode(Edge::Right, ms(0), PanelMode::Docked).unwrap();
    // A transient reveal is visible but is not a stop of the cycle.
    model
        .panel_input(Edge::Bottom, ms(0), PanelInput::Reveal)
        .unwrap();
    model.tick(ms(300)).unwrap();
    // The user clicked into the transient bottom panel, then cycles.
    model.keyboard_focus_observed(Some(Edge::Bottom));
    assert_eq!(model.cycle_keyboard_focus(ms(300)), FocusStop::Panel(Edge::Left));
    assert_eq!(keyboard(&model, Edge::Left), Exclusive);
    assert_eq!(keyboard(&model, Edge::Right), OnDemand);
    assert_eq!(keyboard(&model, Edge::Bottom), OnDemand);

    model.keyboard_focus_observed(Some(Edge::Left));
    assert_eq!(model.cycle_keyboard_focus(ms(300)), FocusStop::Panel(Edge::Right));
    assert_eq!(keyboard(&model, Edge::Left), OnDemand);
    assert_eq!(keyboard(&model, Edge::Right), Exclusive);

    // Past the last persistent panel focus goes back to the application:
    // every panel refuses the keyboard until the host reports it has left.
    model.keyboard_focus_observed(Some(Edge::Right));
    assert_eq!(model.cycle_keyboard_focus(ms(300)), FocusStop::Application);
    assert_eq!(model.focus_directive(), FocusDirective::Release);
    for edge in [Edge::Left, Edge::Right, Edge::Bottom] {
        assert_eq!(keyboard(&model, edge), Refuse);
    }
    model.keyboard_focus_observed(None);
    assert_eq!(model.focus_directive(), FocusDirective::Follow);
    for edge in [Edge::Left, Edge::Right, Edge::Bottom] {
        assert_eq!(keyboard(&model, edge), OnDemand);
    }
    // The walk only ever moved focus.
    assert_eq!(model.panel(Edge::Left).mode, PanelMode::Pinned);
    assert_eq!(model.panel(Edge::Right).mode, PanelMode::Docked);
    assert_eq!(model.panel(Edge::Bottom).mode, PanelMode::Hidden);
    assert!(model.panel(Edge::Bottom).transient_revealed);

    // From the application the walk starts again at the first stop, and a
    // cycle target that unmaps drops its grab rather than keep it for a
    // later hover reveal.
    assert_eq!(model.cycle_keyboard_focus(ms(300)), FocusStop::Panel(Edge::Left));
    model.set_mode(Edge::Left, ms(400), PanelMode::Hidden).unwrap();
    model.tick(ms(1_000)).unwrap();
    assert!(!model.panel(Edge::Left).mapped);
    assert_eq!(model.focus_directive(), FocusDirective::Follow);
}

#[test]
fn escape_on_pinned_or_docked_returns_focus_and_changes_nothing() {
    for mode in [PanelMode::Pinned, PanelMode::Docked] {
        let mut model = model();
        model.set_mode(Edge::Left, ms(0), mode).unwrap();
        // A second, transient panel the Escape is not addressed to.
        model
            .panel_input(Edge::Bottom, ms(0), PanelInput::Reveal)
            .unwrap();
        model.tick(ms(300)).unwrap();
        // The pointer is over the focused panel and its hotspot.
        for input in [PanelInput::CornerEntered, PanelInput::PointerEntered] {
            model.panel_input(Edge::Left, ms(300), input).unwrap();
        }
        model.keyboard_focus_observed(Some(Edge::Left));
        let before = model.panel(Edge::Left);

        let updates = model.escape(ms(310)).unwrap();
        assert_eq!(updates.len(), 1, "{mode:?}: only the focused panel");
        let (edge, update) = updates[0];
        assert_eq!(edge, Edge::Left);
        assert_eq!(update.effect, None, "{mode:?}");
        let after = model.panel(Edge::Left);
        assert_eq!(after.mode, mode, "Escape changed the mode");
        assert_eq!(after.exclusive_zone_px, before.exclusive_zone_px);
        assert_eq!(after.target_fraction, 1.0);
        assert!(!after.transient_revealed);
        assert!(!after.hover_latched, "persistent panels never latch");
        assert!(model.panel(Edge::Bottom).transient_revealed);

        // Focus is handed back, then ordinary policy resumes.
        assert_eq!(model.focus_directive(), FocusDirective::Release);
        assert_eq!(keyboard(&model, Edge::Left), KeyboardInteractivity::None);
        model.keyboard_focus_observed(None);
        assert_eq!(
            keyboard(&model, Edge::Left),
            KeyboardInteractivity::OnDemand
        );
        let frame = ShellFrame::from_model(&model);
        assert_eq!(frame.panel(Edge::Left).mode, mode);
        assert_eq!(
            frame.panel(Edge::Left).exclusive_zone_px,
            before.exclusive_zone_px
        );
    }
}

#[test]
fn escape_on_a_focused_transient_hides_only_that_panel() {
    let mut model = model();
    for edge in [Edge::Left, Edge::Bottom] {
        model.panel_input(edge, ms(0), PanelInput::Reveal).unwrap();
    }
    model.tick(ms(300)).unwrap();
    model.keyboard_focus_observed(Some(Edge::Left));
    model.escape(ms(310)).unwrap();
    assert!(!model.panel(Edge::Left).transient_revealed);
    assert!(model.panel(Edge::Bottom).transient_revealed);
    assert_eq!(model.panel(Edge::Left).mode, PanelMode::Hidden);

    // Without per-panel focus reports (the dev host), Escape reaches every
    // mapped panel as it always has.
    let mut model = self::model();
    for edge in [Edge::Left, Edge::Bottom] {
        model.panel_input(edge, ms(0), PanelInput::Reveal).unwrap();
    }
    model.tick(ms(300)).unwrap();
    model.escape(ms(310)).unwrap();
    assert!(!model.panel(Edge::Left).transient_revealed);
    assert!(!model.panel(Edge::Bottom).transient_revealed);
    assert_eq!(model.focus_directive(), FocusDirective::Follow);

    // Once the host reports focus, "no panel holds it" is an answer, not an
    // absence of one: an Escape then addresses no panel at all.
    let mut model = self::model();
    for edge in [Edge::Left, Edge::Bottom] {
        model.panel_input(edge, ms(0), PanelInput::Reveal).unwrap();
    }
    model.tick(ms(300)).unwrap();
    model.keyboard_focus_observed(Some(Edge::Left));
    model.keyboard_focus_observed(None);
    assert!(model.escape(ms(310)).unwrap().is_empty());
    assert!(model.panel(Edge::Left).transient_revealed);
    assert!(model.panel(Edge::Bottom).transient_revealed);
}

#[test]
fn an_ungranted_cycle_request_expires() {
    let mut model = model();
    model.set_mode(Edge::Left, ms(0), PanelMode::Pinned).unwrap();
    model.tick(ms(300)).unwrap();
    assert_eq!(
        model.cycle_keyboard_focus(ms(300)),
        FocusStop::Panel(Edge::Left)
    );
    assert_eq!(keyboard(&model, Edge::Left), KeyboardInteractivity::Exclusive);
    // The host wakes for the give-up deadline even with nothing animating.
    let deadline = ms(300) + FOCUS_GRANT_TIMEOUT;
    assert_eq!(model.next_deadline(), Some(deadline));
    model.tick(deadline - ms(1)).unwrap();
    assert_eq!(model.focus_directive(), FocusDirective::Panel(Edge::Left));
    // Comp never granted it: the request lapses rather than grab later.
    model.tick(deadline).unwrap();
    assert_eq!(model.focus_directive(), FocusDirective::Follow);
    assert_eq!(keyboard(&model, Edge::Left), KeyboardInteractivity::OnDemand);
    assert_eq!(model.next_deadline(), None);

    // A granted request outlives the deadline, but its grab does not: once
    // the panel holds the keyboard it is on-demand again (changed with named
    // activation, chunk 16), so a click elsewhere can take focus away.
    model.cycle_keyboard_focus(ms(1_000));
    assert_eq!(keyboard(&model, Edge::Left), KeyboardInteractivity::Exclusive);
    model.keyboard_focus_observed(Some(Edge::Left));
    model.tick(ms(5_000)).unwrap();
    assert_eq!(model.focus_directive(), FocusDirective::Panel(Edge::Left));
    assert_eq!(keyboard(&model, Edge::Left), KeyboardInteractivity::OnDemand);
}

/// Click-away after the grant, for both callers of the one focus request:
/// the focus cycle and named activation. Focus landing drops the grab to
/// on-demand; the host then reporting focus gone (a click elsewhere) ends
/// the request, and nothing re-grabs.
#[test]
fn a_granted_focus_request_can_be_clicked_away() {
    for activation in [false, true] {
        let mut model = model();
        model.set_mode(Edge::Left, ms(0), PanelMode::Pinned).unwrap();
        model.tick(ms(300)).unwrap();
        if activation {
            model.request_keyboard_focus(Edge::Left, ms(300));
        } else {
            assert_eq!(model.cycle_keyboard_focus(ms(300)), FocusStop::Panel(Edge::Left));
        }
        let frame = ShellFrame::from_model(&model);
        assert_eq!(frame.panel(Edge::Left).keyboard_interactivity, KeyboardInteractivity::Exclusive);
        assert!(frame.panel(Edge::Left).keyboard_requested && !frame.panel(Edge::Left).keyboard_focused);
        model.keyboard_focus_observed(Some(Edge::Left));
        let frame = ShellFrame::from_model(&model);
        assert_eq!(frame.panel(Edge::Left).keyboard_interactivity, KeyboardInteractivity::OnDemand,
            "activation={activation}: granted, so no longer exclusive");
        assert!(frame.panel(Edge::Left).keyboard_focused);
        model.keyboard_focus_observed(None);
        assert_eq!(model.focus_directive(), FocusDirective::Follow, "activation={activation}");
        let frame = ShellFrame::from_model(&model);
        assert_eq!(frame.panel(Edge::Left).keyboard_interactivity, KeyboardInteractivity::OnDemand);
        assert!(!frame.panel(Edge::Left).keyboard_requested);
        assert_eq!(model.panel(Edge::Left).mode, PanelMode::Pinned);
    }
}
