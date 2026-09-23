//! Pure Quoin behaviour. Nothing in this module may depend on a UI engine,
//! window system, compositor implementation, ABP, or BUS.

mod carousel;
mod corner;
mod keyboard;
mod motion;
mod panel;
mod shell;
mod subpanel;
mod types;

pub use carousel::{Carousel, CarouselError};
pub use corner::{
    CornerDetector, CornerDetectorConfig, CornerDetectorError, CornerDiagnostics, CornerEvent,
    CornerTrigger, PointerSample,
};
pub use keyboard::{FocusDirective, FocusStop, keyboard_target_output, next_focus_stop};
pub use motion::{MotionError, PanelMotion};
pub use panel::{
    ConcealReason, PanelConfig, PanelConfigError, PanelEffect, PanelInput, PanelMode,
    PanelSnapshot, PanelStateMachine, PanelTimeError, PanelUpdate, PanelWake,
    RESIZE_THICKNESS_RANGE, RevealTrigger,
};
pub use shell::{ShellError, ShellModel};
pub use subpanel::{SubPanelRegistry, SubPanelRegistryError, SubPanelSeat};
pub use types::{
    Corner, Edge, GeometryError, LogicalPoint, LogicalSize, LogicalVector, Orientation, OutputKey,
    OutputKeyError, seed_panel_thickness,
};
