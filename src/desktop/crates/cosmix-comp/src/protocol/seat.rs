//! Compositor-owned input handles and optional observer pose.

use super::{KeyboardHandle, PointerHandle, Seat, Serial, WaylandState, WlSurface};

// Keep in step with cosmix-shell-host/src/runner.rs; no unconditional shared desktop crate.
pub const HUMAN_SEAT_NAME: &str = "cosmix";
pub const AGENT_SEAT_NAME: &str = "cosmix-agent";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SeatKind {
    Human,
    Agent,
}

impl SeatKind {
    pub(crate) fn name(self) -> &'static str {
        match self { Self::Human => "human", Self::Agent => "agent" }
    }
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
    pub kind: SeatKind,
    pub seat: Seat<WaylandState>,
    pub keyboard: KeyboardHandle<WaylandState>,
    pub pointer: PointerHandle<WaylandState>,
    pub last_keyboard_action: Option<(Serial, WlSurface)>,
    /// One recent press per device survives a synthetic release for popup requests.
    pub last_pointer_action: Option<(Serial, WlSurface)>,
    pub popup_grab: Option<smithay::desktop::PopupGrab<WaylandState>>,
    /// Surviving canonical parent used when an agent popup loses its surface.
    pub keyboard_root: Option<WlSurface>,
    #[cfg(feature = "bus")]
    pub last_input_us: Option<u64>,
    /// Agent pointer coordinates become known only when its delivery path moves it.
    #[cfg(feature = "bus")]
    pub pointer_position: Option<(f64, f64)>,
    /// Injected holds are owned by this seat; physical pressed state lives
    /// in its Smithay keyboard/pointer handles.
    #[cfg(feature = "bus")]
    pub held: super::input_injection::Holds,
    #[cfg(feature = "bus")]
    pub delivery: super::input_injection::DeliveryScratch,
    #[allow(dead_code)]
    pub pose: Option<SeatPose>,
}

#[cfg(feature = "bus")]
impl WaylandState {
    pub(super) fn comp_seat(&self, kind: SeatKind) -> &CompSeat {
        match kind { SeatKind::Human => &self.human, SeatKind::Agent => &self.agent }
    }

    pub(super) fn comp_seat_mut(&mut self, kind: SeatKind) -> &mut CompSeat {
        match kind { SeatKind::Human => &mut self.human, SeatKind::Agent => &mut self.agent }
    }
}
