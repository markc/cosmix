use std::time::Duration;

use cosmix_shell::core::{
    Corner, CornerDetector, CornerDetectorConfig, CornerDetectorError, CornerEvent, CornerTrigger,
    LogicalPoint, LogicalSize, LogicalVector, PointerSample,
};

fn ms(value: u64) -> Duration {
    Duration::from_millis(value)
}

fn output() -> LogicalSize {
    LogicalSize::new(1_000.0, 800.0).unwrap()
}

fn detector() -> CornerDetector {
    CornerDetector::new(CornerDetectorConfig::new(12.0, ms(200), 1_500.0).unwrap())
}

fn sample(at_ms: u64, x: f32, y: f32) -> PointerSample {
    PointerSample::new(ms(at_ms), LogicalPoint::new(x, y), output())
}

#[test]
fn dwell_emits_entered_then_left() {
    let mut detector = detector();
    assert!(detector.sample(sample(0, 40.0, 40.0)).unwrap().is_empty());
    assert!(detector.sample(sample(50, 8.0, 8.0)).unwrap().is_empty());
    assert_eq!(
        detector.sample(sample(250, 8.0, 8.0)).unwrap(),
        vec![CornerEvent::Entered {
            corner: Corner::TopLeft,
            dwell: ms(200),
            trigger: CornerTrigger::Dwell,
        }]
    );
    assert_eq!(
        detector.sample(sample(260, 20.0, 20.0)).unwrap(),
        vec![CornerEvent::Left {
            corner: Corner::TopLeft,
        }]
    );
}

#[test]
fn all_four_corners_follow_the_same_dwell_contract() {
    let paths = [
        (Corner::TopLeft, LogicalPoint::new(4.0, 4.0)),
        (Corner::BottomLeft, LogicalPoint::new(4.0, 796.0)),
        (Corner::BottomRight, LogicalPoint::new(996.0, 796.0)),
        (Corner::TopRight, LogicalPoint::new(996.0, 4.0)),
    ];
    for (corner, point) in paths {
        let mut detector = detector();
        detector.sample(sample(0, 500.0, 400.0)).unwrap();
        detector
            .sample(PointerSample::new(ms(10), point, output()))
            .unwrap();
        assert_eq!(
            detector
                .sample(PointerSample::new(ms(210), point, output()))
                .unwrap(),
            vec![CornerEvent::Entered {
                corner,
                dwell: ms(200),
                trigger: CornerTrigger::Dwell,
            }]
        );
    }
}

#[test]
fn fast_transit_through_deadzone_emits_nothing() {
    let mut detector = detector();
    detector.sample(sample(0, 100.0, 100.0)).unwrap();
    assert!(detector.sample(sample(5, 5.0, 5.0)).unwrap().is_empty());
    assert!(detector.sample(sample(10, 20.0, 20.0)).unwrap().is_empty());
    assert_eq!(detector.engaged_corner(), None);
}

#[test]
fn zero_dwell_does_not_bypass_velocity_gate_on_fast_entry() {
    let mut detector =
        CornerDetector::new(CornerDetectorConfig::new(12.0, Duration::ZERO, 1_500.0).unwrap());
    detector.sample(sample(0, 100.0, 100.0)).unwrap();
    assert!(detector.sample(sample(1, 5.0, 5.0)).unwrap().is_empty());
    assert_eq!(detector.engaged_corner(), None);
}

#[test]
fn zero_dwell_first_corner_sample_waits_for_velocity_evidence() {
    let mut detector =
        CornerDetector::new(CornerDetectorConfig::new(12.0, Duration::ZERO, 1_500.0).unwrap());
    assert!(detector.sample(sample(0, 5.0, 5.0)).unwrap().is_empty());
    assert_eq!(detector.engaged_corner(), None);
    assert_eq!(detector.next_deadline(), Some(ms(1)));
}

#[test]
fn one_millisecond_dwell_rejects_two_fast_corner_samples() {
    let mut detector =
        CornerDetector::new(CornerDetectorConfig::new(12.0, ms(1), 1_500.0).unwrap());
    detector.sample(sample(0, 100.0, 100.0)).unwrap();
    assert!(detector.sample(sample(1, 8.0, 8.0)).unwrap().is_empty());
    assert!(detector.sample(sample(2, 1.0, 1.0)).unwrap().is_empty());
    assert_eq!(detector.engaged_corner(), None);
}

#[test]
fn fast_arrival_can_engage_after_a_stationary_dwell() {
    let mut detector = detector();
    detector.sample(sample(0, 100.0, 100.0)).unwrap();
    assert!(detector.sample(sample(1, 5.0, 5.0)).unwrap().is_empty());
    assert_eq!(detector.next_deadline(), Some(ms(201)));
    assert_eq!(
        detector.sample(sample(201, 5.0, 5.0)).unwrap(),
        vec![CornerEvent::Entered {
            corner: Corner::TopLeft,
            dwell: ms(200),
            trigger: CornerTrigger::Dwell,
        }]
    );
}

#[test]
fn velocity_gate_rejects_fast_synthetic_push() {
    let mut detector = detector();
    detector.sample(sample(0, 100.0, 100.0)).unwrap();
    detector.sample(sample(10, 0.0, 0.0)).unwrap();
    let pushed = sample(11, 0.0, 0.0).with_attempted_motion(LogicalVector::new(-2.0, -2.0));
    assert!(detector.sample(pushed).unwrap().is_empty());
    assert!(detector.sample(sample(20, 20.0, 20.0)).unwrap().is_empty());
}

#[test]
fn slow_synthetic_push_engages_before_dwell() {
    let mut detector = detector();
    detector.sample(sample(0, 20.0, 20.0)).unwrap();
    detector.sample(sample(100, 5.0, 5.0)).unwrap();
    detector.sample(sample(150, 0.0, 0.0)).unwrap();
    let pushed = sample(160, 0.0, 0.0).with_attempted_motion(LogicalVector::new(-1.0, -1.0));
    assert_eq!(
        detector.sample(pushed).unwrap(),
        vec![CornerEvent::Entered {
            corner: Corner::TopLeft,
            dwell: ms(60),
            trigger: CornerTrigger::SyntheticPush,
        }]
    );
}

#[test]
fn candidate_abort_does_not_emit_left() {
    let mut detector = detector();
    detector.sample(sample(0, 30.0, 30.0)).unwrap();
    detector.sample(sample(50, 5.0, 5.0)).unwrap();
    assert!(detector.sample(sample(100, 30.0, 30.0)).unwrap().is_empty());
    assert!(detector.leave_output(ms(110)).unwrap().is_empty());
}

#[test]
fn diagnostics_expose_the_stationary_dwell_deadline_without_mutation() {
    let mut detector = detector();
    detector.sample(sample(50, 5.0, 5.0)).unwrap();
    let diagnostics = detector.diagnostics(ms(125));
    assert_eq!(diagnostics.candidate, Some(Corner::TopLeft));
    assert_eq!(diagnostics.engaged, None);
    assert_eq!(diagnostics.dwell_elapsed, Duration::ZERO);
    assert_eq!(diagnostics.dwell_deadline, Some(ms(250)));
    assert_eq!(detector.next_deadline(), Some(ms(250)));
    assert_eq!(detector.engaged_corner(), None);
}

#[test]
fn leaving_output_emits_left_only_after_engagement() {
    let mut detector = detector();
    detector.sample(sample(0, 5.0, 5.0)).unwrap();
    detector.sample(sample(200, 5.0, 5.0)).unwrap();
    assert_eq!(
        detector.leave_output(ms(201)).unwrap(),
        vec![CornerEvent::Left {
            corner: Corner::TopLeft,
        }]
    );
    assert!(detector.leave_output(ms(202)).unwrap().is_empty());
}

#[test]
fn geometry_change_releases_engaged_corner_and_restarts_detection() {
    let mut detector = detector();
    detector.sample(sample(0, 5.0, 5.0)).unwrap();
    detector.sample(sample(200, 5.0, 5.0)).unwrap();
    let resized = LogicalSize::new(1_200.0, 900.0).unwrap();
    assert_eq!(
        detector
            .sample(PointerSample::new(
                ms(210),
                LogicalPoint::new(5.0, 5.0),
                resized,
            ))
            .unwrap(),
        vec![CornerEvent::Left {
            corner: Corner::TopLeft,
        }]
    );
    assert!(
        detector
            .sample(PointerSample::new(
                ms(409),
                LogicalPoint::new(5.0, 5.0),
                resized,
            ))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        detector
            .sample(PointerSample::new(
                ms(410),
                LogicalPoint::new(5.0, 5.0),
                resized,
            ))
            .unwrap(),
        vec![CornerEvent::Entered {
            corner: Corner::TopLeft,
            dwell: ms(200),
            trigger: CornerTrigger::Dwell,
        }]
    );
}

#[test]
fn non_monotonic_samples_are_rejected_without_replacing_last_sample() {
    let mut detector = detector();
    detector.sample(sample(100, 20.0, 20.0)).unwrap();
    assert!(matches!(
        detector.sample(sample(99, 5.0, 5.0)),
        Err(CornerDetectorError::NonMonotonicTime { .. })
    ));
    assert!(detector.sample(sample(101, 5.0, 5.0)).is_ok());
}

#[test]
fn invalid_configuration_and_out_of_bounds_samples_are_rejected() {
    assert!(matches!(
        CornerDetectorConfig::new(0.0, ms(200), 1_500.0),
        Err(CornerDetectorError::InvalidDeadzone(0.0))
    ));
    assert!(matches!(
        CornerDetectorConfig::new(12.0, ms(200), f32::NAN),
        Err(CornerDetectorError::InvalidVelocityGate(value)) if value.is_nan()
    ));
    assert!(matches!(
        detector().sample(sample(0, -1.0, 0.0)),
        Err(CornerDetectorError::PointerOutsideOutput { .. })
    ));
}
