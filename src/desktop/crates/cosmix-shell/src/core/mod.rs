//! Pure Quoin behaviour. Nothing in this module may depend on a UI engine,
//! window system, compositor implementation, ABP, or BUS.

mod carousel;
mod corner;
mod motion;
mod panel;
mod shell;
mod types;

pub use carousel::{Carousel, CarouselError};
pub use corner::{
    CornerDetector, CornerDetectorConfig, CornerDetectorError, CornerDiagnostics, CornerEvent,
    CornerTrigger, PointerSample,
};
pub use motion::{MotionError, PanelMotion};
pub use panel::{
    ConcealReason, PanelConfig, PanelConfigError, PanelEffect, PanelInput, PanelMode,
    PanelSnapshot, PanelStateMachine, PanelTimeError, PanelUpdate, PanelWake, RevealTrigger,
};
pub use shell::{ShellError, ShellModel};
pub use types::{
    Corner, Edge, GeometryError, LogicalPoint, LogicalSize, LogicalVector, Orientation, OutputKey,
    OutputKeyError, seed_panel_thickness,
};
