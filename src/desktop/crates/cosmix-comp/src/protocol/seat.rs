//! Compositor-owned input handles and optional observer pose.

use super::{KeyboardHandle, PointerHandle, Seat, Serial, WaylandState, WlSurface};

// Keep in step with cosmix-shell-host/src/runner.rs; no unconditional shared desktop crate.
pub const HUMAN_SEAT_NAME: &str = "cosmix";
pub const AGENT_SEAT_NAME: &str = "cosmix-agent";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SeatKind {
    Human,
    Agent,
}

/// A rigid pose in scene coordinates. Orientation is an xyzw quaternion.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct RigidPose {
    pub translation: [f32; 3],
    pub orientation: [f32; 4],
}

/// The observer belongs to the seat, independently of its input devices.
/// D1, TODO-comp: a seat owns its camera pose and optional hand/controller poses.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct SeatPose {
    pub camera: RigidPose,
    pub left_controller: Option<RigidPose>,
    pub right_controller: Option<RigidPose>,
}

pub(super) struct CompSeat {
    // The groundwork carries these without creating an observer or agent seat.
    #[allow(dead_code)]
    pub kind: SeatKind,
    pub seat: Seat<WaylandState>,
    pub keyboard: KeyboardHandle<WaylandState>,
    pub pointer: PointerHandle<WaylandState>,
    pub last_keyboard_action: Option<(Serial, WlSurface)>,
    /// Injected holds are owned by this seat; physical pressed state lives
    /// in its Smithay keyboard/pointer handles.
    #[cfg(feature = "bus")]
    pub held: super::input_injection::Holds,
    #[allow(dead_code)]
    pub pose: Option<SeatPose>,
}
