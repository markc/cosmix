//! Requests from content that is not a Bevy text widget to the platform host:
//! text input for a focused custom surface, and the pointer's cursor shape.

use bevy::ecs::message::Message;
use bevy::math::Rect;
use bevy::prelude::{Component, Entity, Resource};

/// On the `InputFocus` entity, asks the host to run text input for it as it
/// does for a focused `EditableText`. Results arrive as [`ExternalImeEvent`].
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct ExternalImeTarget {
    pub enabled: bool,
    pub purpose: ImePurpose,
    /// The caret in window-logical coordinates (the layer surface's own).
    pub cursor: Option<Rect>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ImePurpose {
    #[default]
    Normal,
    Password,
    Terminal,
}

/// Text input for an [`ExternalImeTarget`], in text-input-v3 order within a
/// batch: delete, commit, preedit.
#[derive(Message, Clone, Debug, PartialEq)]
pub struct ExternalImeEvent {
    pub target: Entity,
    pub kind: ExternalImeKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ExternalImeKind {
    Enabled,
    Disabled,
    /// Byte lengths around the cursor (or selection) to delete.
    DeleteSurrounding {
        before: u32,
        after: u32,
    },
    Commit(String),
    /// Empty text clears the composition. `cursor` is a byte range.
    Preedit {
        text: String,
        cursor: Option<(usize, usize)>,
    },
}

/// The shape the pointer should show; the host applies it on change and again
/// after each pointer enter. Content that owns the hovered area writes it with
/// itself as `owner`, and clears it (back to the default) only while it is
/// still the owner, so it never undoes another source's request.
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CursorShapeRequest {
    pub shape: CursorShape,
    pub owner: Option<Entity>,
}

impl CursorShapeRequest {
    pub fn set(&mut self, owner: Entity, shape: CursorShape) {
        *self = Self {
            shape,
            owner: Some(owner),
        };
    }

    /// Resets to the default shape if `owner` made the current request.
    pub fn clear(&mut self, owner: Entity) {
        if self.owner == Some(owner) {
            *self = Self::default();
        }
    }
}

/// The subset of the CSS cursor names that content asks for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum CursorShape {
    #[default]
    Default,
    Pointer,
    Text,
    Grab,
    Grabbing,
    NotAllowed,
    EwResize,
    NsResize,
    Crosshair,
    Wait,
}
