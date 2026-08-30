//! Renderer/window-system seam for Quoin.
//!
//! Q-0 defines only the pure contract. The one-window development host and the
//! later layer-shell host implement the same surface in subsequent rungs.

use crate::core::Edge;
use crate::runtime::{HostGeometry, ShellFrame, WakePolicy};

/// Presentation host for one output's four panel mounts.
pub trait ShellHost {
    type Error;
    type Mount: Copy + Eq;

    fn geometry(&self) -> &HostGeometry;
    fn panel_mount(&self, edge: Edge) -> Self::Mount;
    fn apply(&mut self, frame: &ShellFrame) -> Result<(), Self::Error>;
    fn set_wake_policy(&mut self, policy: WakePolicy) -> Result<(), Self::Error>;
}
