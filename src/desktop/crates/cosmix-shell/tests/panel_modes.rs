use std::time::Duration;

use cosmix_shell::core::{PanelConfig, PanelEffect, PanelInput, PanelMode, PanelStateMachine};

fn ms(value: u64) -> Duration {
    Duration::from_millis(value)
}

fn panel(mode: PanelMode, transient: bool) -> PanelStateMachine {
    let mut panel =
        PanelStateMachine::new(PanelConfig::new(200.0, ms(800), ms(200)).unwrap(), ms(0)).unwrap();
    panel.apply(ms(0), PanelInput::SetMode(mode)).unwrap();
    if transient {
        panel.apply(ms(0), PanelInput::Reveal).unwrap();
    }
    panel.tick(ms(200)).unwrap();
    panel
}

#[test]
fn every_mode_and_transient_visibility_cross_every_input() {
    use PanelInput::*;
    use PanelMode::{Docked, Hidden, Pinned};
    // These are test expectations, not additional runtime enum states.
    let states = [
        (Hidden, false),
        (Hidden, true),
        (Pinned, false),
        (Docked, false),
    ];
    // Each row lists the expected state index for the four starting conditions.
    let cases = [
        (Reveal, [1, 1, 2, 3]),
        (Toggle, [1, 0, 2, 3]),
        (CornerEntered, [1, 1, 2, 3]),
        (CornerLeft, [0, 1, 2, 3]),
        (Hide, [0, 0, 2, 3]),
        (Escape, [0, 0, 2, 3]),
        (Pin, [2, 2, 2, 2]),
        (Unpin, [0, 1, 1, 3]),
        (PinToggle, [2, 2, 1, 2]),
        (Dock, [3, 3, 3, 3]),
        (Undock, [0, 1, 2, 1]),
        (DockToggle, [3, 3, 3, 1]),
        (Release, [0, 1, 1, 1]),
        (SetMode(Hidden), [0, 0, 0, 0]),
        (SetMode(Pinned), [2, 2, 2, 2]),
        (SetMode(Docked), [3, 3, 3, 3]),
        (PointerEntered, [0, 1, 2, 3]),
        (PointerLeft, [0, 1, 2, 3]),
        (ResizeStarted, [0, 1, 2, 3]),
        (ResizeCompleted, [0, 1, 2, 3]),
        (ResizeCancelled, [0, 1, 2, 3]),
    ];
    for (input, expected) in cases {
        for (index, (mode, transient)) in states.into_iter().enumerate() {
            let mut panel = panel(mode, transient);
            let update = panel.apply(ms(200), input).unwrap();
            let snapshot = update.snapshot;
            let (next_mode, next_transient) = states[expected[index]];
            assert_eq!(
                (snapshot.mode, snapshot.transient_revealed),
                (next_mode, next_transient),
                "{mode:?}, transient={transient}, input={input:?}"
            );
            assert_eq!(
                snapshot.target_fraction,
                if next_mode != Hidden || next_transient {
                    1.0
                } else {
                    0.0
                }
            );
            assert_eq!(
                snapshot.exclusive_zone_px,
                if next_mode == Docked { 200.0 } else { 0.0 }
            );
            assert_eq!(
                snapshot.mapped,
                next_mode != Hidden || next_transient || snapshot.visible_fraction > 0.0
            );
            assert_eq!(
                update
                    .effect
                    .filter(|effect| matches!(effect, PanelEffect::ModeChanged { .. })),
                (mode != next_mode).then_some(PanelEffect::ModeChanged { mode: next_mode }),
                "persistence effect for {mode:?} / {input:?}"
            );
            if next_mode != Hidden {
                assert_eq!(snapshot.hide_at, None);
            }
        }
    }
}

#[test]
fn holds_grace_intro_and_resize_never_hide_persistent_modes() {
    for mode in [PanelMode::Hidden, PanelMode::Pinned, PanelMode::Docked] {
        let mut panel = panel(mode, mode == PanelMode::Hidden);
        panel.start_intro(ms(100));
        panel.apply(ms(200), PanelInput::CornerEntered).unwrap();
        panel.apply(ms(200), PanelInput::PointerEntered).unwrap();
        panel.apply(ms(200), PanelInput::ResizeStarted).unwrap();
        panel.apply(ms(200), PanelInput::CornerLeft).unwrap();
        panel.apply(ms(200), PanelInput::PointerLeft).unwrap();
        panel.tick(ms(2000)).unwrap();
        assert_eq!(panel.snapshot().hide_at, None);
        assert_eq!(panel.snapshot().mode, mode);
        panel.apply(ms(2000), PanelInput::ResizeCompleted).unwrap();
        assert_eq!(
            panel.snapshot().hide_at,
            (mode == PanelMode::Hidden).then_some(ms(2800))
        );
        let update = panel.tick(ms(3000)).unwrap();
        assert_eq!(update.snapshot.mode, mode);
        assert!(!update.snapshot.transient_revealed);
        assert_eq!(update.snapshot.mapped, mode != PanelMode::Hidden);
        assert!(!matches!(
            update.effect,
            Some(PanelEffect::ModeChanged { .. })
        ));
    }
}

#[test]
fn ordinary_holds_and_deadlines_emit_no_persistent_effects() {
    let mut panel = panel(PanelMode::Hidden, false);
    for (at, input) in [
        (200, PanelInput::CornerEntered),
        (300, PanelInput::PointerEntered),
        (400, PanelInput::CornerLeft),
        (500, PanelInput::PointerLeft),
    ] {
        let update = panel.apply(ms(at), input).unwrap();
        assert_eq!(update.snapshot.mode, PanelMode::Hidden);
        assert!(!matches!(
            update.effect,
            Some(PanelEffect::ModeChanged { .. })
        ));
    }
    let update = panel.tick(ms(1500)).unwrap();
    assert!(!update.snapshot.mapped);
    assert!(!update.snapshot.transient_revealed);
    assert!(!matches!(
        update.effect,
        Some(PanelEffect::ModeChanged { .. })
    ));
}

#[test]
fn mode_changes_cancel_intro_and_stale_deadlines() {
    for mode in [PanelMode::Pinned, PanelMode::Docked] {
        let mut panel = panel(PanelMode::Hidden, true);
        panel.start_intro(ms(300));
        panel.apply(ms(200), PanelInput::CornerEntered).unwrap();
        panel.apply(ms(200), PanelInput::CornerLeft).unwrap();
        panel.apply(ms(200), PanelInput::SetMode(mode)).unwrap();
        panel.tick(ms(5000)).unwrap();
        assert_eq!(panel.snapshot().mode, mode);
        assert_eq!(panel.snapshot().hide_at, None);
        assert!(!panel.snapshot().transient_revealed);
        assert_eq!(panel.next_deadline(), None);
    }
}
