//! Logical geometry and four-edge vocabulary for Quoin.
//!
//! Panel thickness seeding implements
//! `_plan/2026-08-06-cosmix-shell-corner-panels.md` §E2. Percentages are used
//! only to choose the first logical-pixel value; callers retain that value as
//! authoritative afterwards.

use std::error::Error;
use std::fmt::{Display, Formatter};

/// One physical edge of an output.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Edge {
    Left,
    Bottom,
    Right,
    Top,
}

impl Edge {
    pub const ALL: [Self; 4] = [Self::Left, Self::Bottom, Self::Right, Self::Top];

    pub const fn index(self) -> usize {
        match self {
            Self::Left => 0,
            Self::Bottom => 1,
            Self::Right => 2,
            Self::Top => 3,
        }
    }

    pub const fn orientation(self) -> Orientation {
        match self {
            Self::Left | Self::Right => Orientation::Vertical,
            Self::Top | Self::Bottom => Orientation::Horizontal,
        }
    }
}

/// Layout direction of an edge panel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Orientation {
    Horizontal,
    Vertical,
}

/// A physical corner of an output.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Corner {
    TopLeft,
    BottomLeft,
    BottomRight,
    TopRight,
}

impl Corner {
    pub const ALL: [Self; 4] = [
        Self::TopLeft,
        Self::BottomLeft,
        Self::BottomRight,
        Self::TopRight,
    ];

    /// The edge at this corner's clockwise end, per the Quoin mapping rule.
    pub const fn summoned_edge(self) -> Edge {
        match self {
            Self::TopLeft => Edge::Left,
            Self::BottomLeft => Edge::Bottom,
            Self::BottomRight => Edge::Right,
            Self::TopRight => Edge::Top,
        }
    }
}

/// A logical-pixel point relative to an output's top-left corner.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LogicalPoint {
    pub x: f32,
    pub y: f32,
}

impl LogicalPoint {
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

/// A logical-pixel motion vector.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LogicalVector {
    pub x: f32,
    pub y: f32,
}

impl LogicalVector {
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    pub fn length(self) -> f32 {
        self.x.hypot(self.y)
    }
}

/// Valid positive logical dimensions of one output.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LogicalSize {
    width: f32,
    height: f32,
}

impl LogicalSize {
    pub fn new(width: f32, height: f32) -> Result<Self, GeometryError> {
        if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
            return Err(GeometryError { width, height });
        }
        Ok(Self { width, height })
    }

    pub const fn width(self) -> f32 {
        self.width
    }

    pub const fn height(self) -> f32 {
        self.height
    }

    pub fn contains(self, point: LogicalPoint) -> bool {
        point.x.is_finite()
            && point.y.is_finite()
            && point.x >= 0.0
            && point.y >= 0.0
            && point.x <= self.width
            && point.y <= self.height
    }
}

/// Invalid logical output dimensions.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GeometryError {
    pub width: f32,
    pub height: f32,
}

impl Display for GeometryError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "logical output size must be finite and positive, got {}x{}",
            self.width, self.height
        )
    }
}

impl Error for GeometryError {}

/// Stable shell-side identity for one output.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct OutputKey(String);

impl OutputKey {
    pub fn new(value: impl Into<String>) -> Result<Self, OutputKeyError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(OutputKeyError);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// An output key was empty or whitespace-only.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputKeyError;

impl Display for OutputKeyError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("output key must not be empty")
    }
}

impl Error for OutputKeyError {}

/// Seed an edge panel's stored logical-pixel thickness from output geometry.
pub fn seed_panel_thickness(edge: Edge, output: LogicalSize) -> f32 {
    match edge {
        Edge::Left | Edge::Right => (output.width() * 0.15).clamp(240.0, 480.0),
        Edge::Top => (output.height() * 0.05).clamp(32.0, 64.0),
        Edge::Bottom => (output.height() * 0.10).clamp(56.0, 128.0),
    }
}
