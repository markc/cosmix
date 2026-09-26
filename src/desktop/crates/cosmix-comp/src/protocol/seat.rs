//! Compositor-owned input handles and optional observer pose.

use super::{KeyboardHandle, PointerHandle, Seat, WaylandState};

// Keep in step with cosmix-shell-host/src/runner.rs; no unconditional shared desktop crate.
pub const HUMAN_SEAT_NAME: &str = "cosmix";
#[allow(dead_code)] // The agent global arrives in the next chunk.
pub const AGENT_SEAT_NAME: &str = "cosmix-agent";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SeatKind {
    Human,
    // Reserved for the separate injected-input seat; no second global yet.
    #[allow(dead_code)]
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
    #[allow(dead_code)]
    pub pose: Option<SeatPose>,
}
