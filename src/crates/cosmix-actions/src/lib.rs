//! Shared, engine-independent action naming, registration and key resolution.
//!
//! `cosmix-actions` is the core action spine used by CTK menus and apps.  It
//! deliberately has no Bevy, Bus or mesh dependency.  Rendering engines
//! translate their keyboard events into [`RawInput`]; apps publish serialisable
//! [`ActionMeta`] while retaining handlers and enabled predicates locally in an
//! [`ActionRegistry`].
//!
//! Keymaps are strict-data `.mix`, parsed and emitted only through
//! `cosmix-lib-config`'s `cosmix_mix` serde bridge.  Resolution is deterministic
//! and testable: callers supply a monotonic [`Tick`] rather than letting this
//! crate read a wall clock.
//!
//! Dynamic ids are validated before process-lifetime interning: at most
//! [`MAX_ACTION_ID_LEN`] bytes per id, [`MAX_KEYMAP_ACTION_IDS`] distinct ids,
//! [`MAX_KEYMAP_BINDINGS`] total entries and [`MAX_CHORD_STROKES`] strokes per
//! chord, with [`MAX_INTERNED_ACTION_IDS`] dynamic ids per process.

#![warn(missing_docs)]

mod action;
pub mod filemgr;
mod input;
mod keymap;
mod registry;
mod resolve;
pub mod studio;
pub mod theme;

pub use action::{ActionId, ActionIdError, MAX_ACTION_ID_LEN, MAX_INTERNED_ACTION_IDS};
pub use input::{
    BindingScope, Chord, FocusContext, Key, KeyParseError, KeyStroke, MAX_CHORD_STROKES, Modifiers,
    RawInput, RawInputState, RepeatPolicy, Tick,
};
pub use keymap::{
    Binding, BindingLayer, BindingOverride, EffectiveBinding, KEYMAP_SCHEMA_VERSION, Keymap,
    KeymapDiagnostic, KeymapError, MAX_KEYMAP_ACTION_IDS, MAX_KEYMAP_BINDINGS,
    MAX_KEYMAP_FILE_BYTES, load_keymap, parse_keymap, save_keymap, to_keymap_mix,
};
pub use registry::{
    ActionArg, ActionArgKind, ActionArgs, ActionHandler, ActionHandlerResult, ActionMeta,
    ActionMetadata, ActionMetadataError, ActionRegistry, ActionSource, ActionSources, ActionValue,
    ArgsSchema, EnabledPredicate, InteractiveAction, MAX_ACTION_ARGUMENT_FIELDS,
    MAX_ACTION_METADATA_BYTES, MAX_ACTION_METADATA_ITEMS, MAX_ACTION_REGISTRY_BYTES,
    MAX_ACTION_REGISTRY_ITEMS, RegistryError, parse_action_metadata, to_action_metadata_mix,
};
pub use resolve::{
    PendingInvalidation, ResolveDiagnostic, ResolveOutcome, ResolveState, Resolved,
    SuppressionReason, resolve, resolve_timeout,
};

/// Studio's checked-in built-in keymap, suitable for [`parse_keymap`].
pub const STUDIO_DEFAULT_KEYMAP_MIX: &str = include_str!("../assets/studio-default-keymap.mix");

/// FileMgr's checked-in built-in keymap, suitable for [`parse_keymap`].
pub const FILEMGR_DEFAULT_KEYMAP_MIX: &str = include_str!("../assets/filemgr-default-keymap.mix");

#[cfg(test)]
mod tests;
