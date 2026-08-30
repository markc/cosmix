use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::Read;
use std::marker::PhantomData;
use std::path::Path;

use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::{ActionId, BindingScope, Chord, RepeatPolicy};

/// Current strict-data keymap schema version.
pub const KEYMAP_SCHEMA_VERSION: u32 = 1;

/// Maximum distinct action ids accepted from one keymap before interning.
pub const MAX_KEYMAP_ACTION_IDS: usize = 1_024;

/// Maximum default plus custom entries accepted in one keymap.
pub const MAX_KEYMAP_BINDINGS: usize = 4_096;

/// Maximum UTF-8 bytes accepted at a keymap load or parse boundary.
pub const MAX_KEYMAP_FILE_BYTES: usize = 1_048_576;

/// One app-shipped default action binding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    /// Action emitted when the chord resolves.
    pub action: ActionId,
    /// Ordered physical key strokes.
    pub chord: Chord,
    /// Global, modal, or app-defined focus scope.
    #[serde(default)]
    pub scope: BindingScope,
    /// Operating-system repeat behaviour.
    #[serde(default)]
    pub repeat: RepeatPolicy,
    /// Whether this command may resolve while an editable widget has focus.
    #[serde(default)]
    pub allow_in_editable: bool,
}

/// A per-app user override of built-in bindings.
///
/// The presence of any override for an `(action, scope)` removes all defaults
/// for that pair. `chord: nil` therefore unbinds it; multiple non-nil entries
/// may give an action several custom chords.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BindingOverride {
    /// Action whose defaults are replaced.
    pub action: ActionId,
    /// Replacement chord, or `nil` to leave the action unbound in this scope.
    pub chord: Option<Chord>,
    /// Scope whose defaults are replaced.
    #[serde(default)]
    pub scope: BindingScope,
    /// Operating-system repeat behaviour for a replacement chord.
    #[serde(default)]
    pub repeat: RepeatPolicy,
    /// Whether a replacement may resolve while an editable has focus.
    #[serde(default)]
    pub allow_in_editable: bool,
}

/// Layered strict-data keymap: built-ins followed by per-app user overrides.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Keymap {
    /// File schema version; currently [`KEYMAP_SCHEMA_VERSION`].
    pub version: u32,
    /// Maximum milliseconds between accepted chord strokes.
    pub chord_timeout_ms: u64,
    /// App-shipped default bindings.
    #[serde(default)]
    pub defaults: Vec<Binding>,
    /// User customisations applied over defaults.
    #[serde(default)]
    pub custom: Vec<BindingOverride>,
}

impl Default for Keymap {
    fn default() -> Self {
        Self {
            version: KEYMAP_SCHEMA_VERSION,
            chord_timeout_ms: 1_000,
            defaults: Vec::new(),
            custom: Vec::new(),
        }
    }
}

impl Keymap {
    /// Check schema, timeout, entry-count, id-count, chord and scope invariants.
    pub fn validate(&self) -> Result<(), KeymapError> {
        if self.version != KEYMAP_SCHEMA_VERSION {
            return Err(KeymapError::UnsupportedVersion(self.version));
        }
        if self.chord_timeout_ms == 0 || self.chord_timeout_ms > 60_000 {
            return Err(KeymapError::InvalidTimeout(self.chord_timeout_ms));
        }
        let binding_count = self.defaults.len().saturating_add(self.custom.len());
        if binding_count > MAX_KEYMAP_BINDINGS {
            return Err(KeymapError::TooManyBindings {
                count: binding_count,
                maximum: MAX_KEYMAP_BINDINGS,
            });
        }
        let distinct: BTreeSet<_> = self
            .defaults
            .iter()
            .map(|binding| binding.action)
            .chain(self.custom.iter().map(|binding| binding.action))
            .collect();
        if distinct.len() > MAX_KEYMAP_ACTION_IDS {
            return Err(KeymapError::TooManyActionIds {
                count: distinct.len(),
                maximum: MAX_KEYMAP_ACTION_IDS,
            });
        }
        for binding in &self.defaults {
            validate_binding(binding.action, &binding.chord, &binding.scope)?;
        }
        for binding in &self.custom {
            binding
                .scope
                .validate()
                .map_err(|error| KeymapError::InvalidBinding {
                    action: binding.action,
                    reason: error.to_string(),
                })?;
            if let Some(chord) = &binding.chord {
                validate_binding(binding.action, chord, &binding.scope)?;
            }
        }
        Ok(())
    }

    /// Return the preferred global chord display for an action.
    ///
    /// This is the menu accelerator-hint API. It returns only a global chord
    /// whose exact non-repeated, non-editable resolution has this action as
    /// its sole highest-layer winner. Shadowed and same-layer-conflicted
    /// chords are omitted rather than advertising an action that will not run.
    pub fn binding_for(&self, action: ActionId) -> Option<String> {
        self.effective_bindings()
            .filter(|binding| binding.action == action && *binding.scope == BindingScope::Global)
            .find(|candidate| self.global_exact_winners(candidate.chord) == [action])
            .map(|binding| binding.chord.to_string())
    }

    /// Iterate the resolved default/custom binding layer consumed by resolvers.
    ///
    /// Any custom entry removes defaults for the same `(action, scope)` before
    /// this iterator is produced. Consumers should use this API rather than
    /// reimplementing keymap layering.
    pub fn effective_bindings(&self) -> impl ExactSizeIterator<Item = EffectiveBinding<'_>> + '_ {
        self.collect_effective_bindings().into_iter()
    }

    /// Inspect replacement, exact-shadowing and equal-priority conflicts.
    pub fn diagnostics(&self) -> Vec<KeymapDiagnostic> {
        let mut diagnostics = Vec::new();
        let replaced: BTreeSet<_> = self
            .custom
            .iter()
            .map(|binding| (binding.action, binding.scope.clone()))
            .collect();
        for (action, scope) in &replaced {
            if self
                .defaults
                .iter()
                .any(|binding| binding.action == *action && binding.scope == *scope)
            {
                diagnostics.push(KeymapDiagnostic::DefaultReplaced {
                    action: *action,
                    scope: scope.clone(),
                    unbound: !self.custom.iter().any(|binding| {
                        binding.action == *action
                            && binding.scope == *scope
                            && binding.chord.is_some()
                    }),
                });
            }
        }

        let mut exact_groups: BTreeMap<_, Vec<_>> = BTreeMap::new();
        for binding in self.effective_bindings() {
            exact_groups
                .entry((binding.chord.clone(), binding.scope.clone()))
                .or_default()
                .push(binding);
        }
        for ((chord, scope), bindings) in exact_groups {
            let maximum_layer = bindings
                .iter()
                .map(|binding| binding.layer.rank())
                .max()
                .expect("exact group is non-empty");
            let winners = unique_ids(
                bindings
                    .iter()
                    .filter(|binding| binding.layer.rank() == maximum_layer)
                    .map(|binding| binding.action),
            );
            if winners.len() > 1 {
                diagnostics.push(KeymapDiagnostic::Conflict {
                    chord: chord.clone(),
                    scope: scope.clone(),
                    layer: bindings
                        .iter()
                        .find(|binding| binding.layer.rank() == maximum_layer)
                        .expect("winner exists")
                        .layer,
                    actions: winners.clone(),
                });
            }
            let lower_actions = unique_ids(
                bindings
                    .iter()
                    .filter(|binding| binding.layer.rank() < maximum_layer)
                    .map(|binding| binding.action),
            );
            let winner_bindings: Vec<_> = bindings
                .iter()
                .filter(|binding| binding.layer.rank() == maximum_layer)
                .collect();
            for shadowed in lower_actions {
                let fully_covered = bindings
                    .iter()
                    .filter(|binding| {
                        binding.layer.rank() < maximum_layer && binding.action == shadowed
                    })
                    .all(|lower| binding_fully_covered(lower, &winner_bindings));
                if !winners.contains(&shadowed) && fully_covered {
                    diagnostics.push(KeymapDiagnostic::Shadowed {
                        chord: chord.clone(),
                        scope: scope.clone(),
                        shadowed,
                        by: winners.clone(),
                    });
                }
            }
        }
        diagnostics
    }

    fn collect_effective_bindings(&self) -> Vec<EffectiveBinding<'_>> {
        let replaced: BTreeSet<_> = self
            .custom
            .iter()
            .map(|binding| (binding.action, binding.scope.clone()))
            .collect();
        let defaults = self.defaults.iter().filter_map(|binding| {
            (!replaced.contains(&(binding.action, binding.scope.clone()))).then_some(
                EffectiveBinding {
                    action: binding.action,
                    chord: &binding.chord,
                    scope: &binding.scope,
                    repeat: binding.repeat,
                    allow_in_editable: binding.allow_in_editable,
                    layer: BindingLayer::Default,
                },
            )
        });
        let custom = self.custom.iter().filter_map(|binding| {
            binding.chord.as_ref().map(|chord| EffectiveBinding {
                action: binding.action,
                chord,
                scope: &binding.scope,
                repeat: binding.repeat,
                allow_in_editable: binding.allow_in_editable,
                layer: BindingLayer::Custom,
            })
        });
        defaults.chain(custom).collect()
    }

    fn global_exact_winners(&self, chord: &Chord) -> Vec<ActionId> {
        let exact: Vec<_> = self
            .effective_bindings()
            .filter(|binding| binding.chord == chord && *binding.scope == BindingScope::Global)
            .collect();
        let Some(maximum_layer) = exact.iter().map(|binding| binding.layer.rank()).max() else {
            return Vec::new();
        };
        unique_ids(
            exact
                .into_iter()
                .filter(|binding| binding.layer.rank() == maximum_layer)
                .map(|binding| binding.action),
        )
    }
}

fn validate_binding(
    action: ActionId,
    chord: &Chord,
    scope: &BindingScope,
) -> Result<(), KeymapError> {
    chord
        .validate()
        .and_then(|()| scope.validate())
        .map_err(|error| KeymapError::InvalidBinding {
            action,
            reason: error.to_string(),
        })
}

fn unique_ids(ids: impl Iterator<Item = ActionId>) -> Vec<ActionId> {
    let mut ids: Vec<_> = ids.collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

fn binding_fully_covered(lower: &EffectiveBinding<'_>, winners: &[&EffectiveBinding<'_>]) -> bool {
    [false, true].into_iter().all(|repeated| {
        [false, true].into_iter().all(|editable| {
            !binding_eligible(lower, repeated, editable)
                || winners
                    .iter()
                    .any(|winner| binding_eligible(winner, repeated, editable))
        })
    })
}

fn binding_eligible(binding: &EffectiveBinding<'_>, repeated: bool, editable: bool) -> bool {
    (!repeated || binding.repeat == RepeatPolicy::Allow) && (!editable || binding.allow_in_editable)
}

/// Origin layer of an [`EffectiveBinding`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum BindingLayer {
    /// App-shipped default.
    Default,
    /// Per-app user customisation.
    Custom,
}

impl BindingLayer {
    pub(crate) const fn rank(self) -> u8 {
        match self {
            Self::Default => 0,
            Self::Custom => 1,
        }
    }
}

/// One binding after same-action custom replacement has been applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EffectiveBinding<'a> {
    /// Action emitted by this binding.
    pub action: ActionId,
    /// Ordered key sequence.
    pub chord: &'a Chord,
    /// Binding scope.
    pub scope: &'a BindingScope,
    /// Key-repeat policy.
    pub repeat: RepeatPolicy,
    /// Whether editable focus admits it.
    pub allow_in_editable: bool,
    /// Default or custom origin.
    pub layer: BindingLayer,
}

/// Static layering issue discoverable without processing input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeymapDiagnostic {
    /// Custom entries replaced an action's defaults in one scope.
    DefaultReplaced {
        /// Replaced action.
        action: ActionId,
        /// Affected scope.
        scope: BindingScope,
        /// Whether the custom layer leaves the action unbound.
        unbound: bool,
    },
    /// An exact lower-layer binding can never win in the same scope.
    Shadowed {
        /// Shared exact chord.
        chord: Chord,
        /// Shared scope.
        scope: BindingScope,
        /// Unreachable action.
        shadowed: ActionId,
        /// Higher-layer winner actions.
        by: Vec<ActionId>,
    },
    /// Equal-priority exact bindings name different actions.
    Conflict {
        /// Shared exact chord.
        chord: Chord,
        /// Shared scope.
        scope: BindingScope,
        /// Conflicting layer.
        layer: BindingLayer,
        /// Conflicting action ids.
        actions: Vec<ActionId>,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBinding {
    action: String,
    chord: Chord,
    #[serde(default)]
    scope: BindingScope,
    #[serde(default)]
    repeat: RepeatPolicy,
    #[serde(default)]
    allow_in_editable: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBindingOverride {
    action: String,
    chord: Option<Chord>,
    #[serde(default)]
    scope: BindingScope,
    #[serde(default)]
    repeat: RepeatPolicy,
    #[serde(default)]
    allow_in_editable: bool,
}

#[derive(Deserialize)]
struct VersionEnvelope {
    version: u32,
}

struct RawKeymap {
    version: u32,
    chord_timeout_ms: u64,
    defaults: Vec<RawBinding>,
    custom: Vec<RawBindingOverride>,
}

struct BoundedVecSeed<T> {
    maximum: usize,
    marker: PhantomData<T>,
}

impl<T> BoundedVecSeed<T> {
    const fn new(maximum: usize) -> Self {
        Self {
            maximum,
            marker: PhantomData,
        }
    }
}

impl<'de, T> DeserializeSeed<'de> for BoundedVecSeed<T>
where
    T: Deserialize<'de>,
{
    type Value = Vec<T>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct BoundedVecVisitor<T> {
            maximum: usize,
            marker: PhantomData<T>,
        }

        impl<'de, T> Visitor<'de> for BoundedVecVisitor<T>
        where
            T: Deserialize<'de>,
        {
            type Value = Vec<T>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, "at most {} keymap bindings", self.maximum)
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                if sequence.size_hint().is_some_and(|size| size > self.maximum) {
                    return Err(serde::de::Error::custom(format_args!(
                        "keymap exceeds the combined limit of {MAX_KEYMAP_BINDINGS} bindings"
                    )));
                }
                let mut items =
                    Vec::with_capacity(sequence.size_hint().unwrap_or(0).min(self.maximum));
                while let Some(item) = sequence.next_element()? {
                    if items.len() == self.maximum {
                        return Err(serde::de::Error::custom(format_args!(
                            "keymap exceeds the combined limit of {MAX_KEYMAP_BINDINGS} bindings"
                        )));
                    }
                    items.push(item);
                }
                Ok(items)
            }
        }

        deserializer.deserialize_seq(BoundedVecVisitor {
            maximum: self.maximum,
            marker: PhantomData,
        })
    }
}

impl<'de> Deserialize<'de> for RawKeymap {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct RawKeymapVisitor;

        impl<'de> Visitor<'de> for RawKeymapVisitor {
            type Value = RawKeymap;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a strict version 1 keymap object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut version = None;
                let mut chord_timeout_ms = None;
                let mut defaults: Option<Vec<RawBinding>> = None;
                let mut custom: Option<Vec<RawBindingOverride>> = None;
                while let Some(field) = map.next_key::<String>()? {
                    match field.as_str() {
                        "version" => {
                            if version.is_some() {
                                return Err(serde::de::Error::duplicate_field("version"));
                            }
                            version = Some(map.next_value()?);
                        }
                        "chord_timeout_ms" => {
                            if chord_timeout_ms.is_some() {
                                return Err(serde::de::Error::duplicate_field("chord_timeout_ms"));
                            }
                            chord_timeout_ms = Some(map.next_value()?);
                        }
                        "defaults" => {
                            if defaults.is_some() {
                                return Err(serde::de::Error::duplicate_field("defaults"));
                            }
                            let remaining = MAX_KEYMAP_BINDINGS
                                .saturating_sub(custom.as_ref().map_or(0, Vec::len));
                            defaults = Some(map.next_value_seed(BoundedVecSeed::new(remaining))?);
                        }
                        "custom" => {
                            if custom.is_some() {
                                return Err(serde::de::Error::duplicate_field("custom"));
                            }
                            let remaining = MAX_KEYMAP_BINDINGS
                                .saturating_sub(defaults.as_ref().map_or(0, Vec::len));
                            custom = Some(map.next_value_seed(BoundedVecSeed::new(remaining))?);
                        }
                        _ => {
                            return Err(serde::de::Error::unknown_field(
                                &field,
                                &["version", "chord_timeout_ms", "defaults", "custom"],
                            ));
                        }
                    }
                }
                Ok(RawKeymap {
                    version: version.ok_or_else(|| serde::de::Error::missing_field("version"))?,
                    chord_timeout_ms: chord_timeout_ms
                        .ok_or_else(|| serde::de::Error::missing_field("chord_timeout_ms"))?,
                    defaults: defaults.unwrap_or_default(),
                    custom: custom.unwrap_or_default(),
                })
            }
        }

        deserializer.deserialize_map(RawKeymapVisitor)
    }
}

fn keymap_from_raw(raw: RawKeymap) -> Result<Keymap, KeymapError> {
    if raw.version != KEYMAP_SCHEMA_VERSION {
        return Err(KeymapError::UnsupportedVersion(raw.version));
    }
    if raw.chord_timeout_ms == 0 || raw.chord_timeout_ms > 60_000 {
        return Err(KeymapError::InvalidTimeout(raw.chord_timeout_ms));
    }
    let binding_count = raw.defaults.len().saturating_add(raw.custom.len());
    if binding_count > MAX_KEYMAP_BINDINGS {
        return Err(KeymapError::TooManyBindings {
            count: binding_count,
            maximum: MAX_KEYMAP_BINDINGS,
        });
    }

    let action_names: Vec<_> = raw
        .defaults
        .iter()
        .map(|binding| binding.action.as_str())
        .chain(raw.custom.iter().map(|binding| binding.action.as_str()))
        .collect();
    for action in &action_names {
        ActionId::validate_str(action)
            .map_err(|error| KeymapError::InvalidActionId(error.to_string()))?;
    }
    let distinct: BTreeSet<_> = action_names.iter().copied().collect();
    if distinct.len() > MAX_KEYMAP_ACTION_IDS {
        return Err(KeymapError::TooManyActionIds {
            count: distinct.len(),
            maximum: MAX_KEYMAP_ACTION_IDS,
        });
    }
    for binding in &raw.defaults {
        binding
            .chord
            .validate()
            .and_then(|()| binding.scope.validate())
            .map_err(|error| KeymapError::InvalidRawBinding(error.to_string()))?;
    }
    for binding in &raw.custom {
        binding
            .scope
            .validate()
            .and_then(|()| binding.chord.as_ref().map_or(Ok(()), Chord::validate))
            .map_err(|error| KeymapError::InvalidRawBinding(error.to_string()))?;
    }

    // The whole file has survived grammar, count, chord and scope validation.
    // Batch interning checks process capacity before leaking any new string.
    let interned = ActionId::intern_many(&action_names)
        .map_err(|error| KeymapError::InvalidActionId(error.to_string()))?;
    drop(action_names);
    let mut ids = interned.into_iter();
    let defaults = raw
        .defaults
        .into_iter()
        .map(|binding| Binding {
            action: ids.next().expect("one interned id per raw binding"),
            chord: binding.chord,
            scope: binding.scope,
            repeat: binding.repeat,
            allow_in_editable: binding.allow_in_editable,
        })
        .collect();
    let custom = raw
        .custom
        .into_iter()
        .map(|binding| BindingOverride {
            action: ids.next().expect("one interned id per raw binding"),
            chord: binding.chord,
            scope: binding.scope,
            repeat: binding.repeat,
            allow_in_editable: binding.allow_in_editable,
        })
        .collect();
    let keymap = Keymap {
        version: raw.version,
        chord_timeout_ms: raw.chord_timeout_ms,
        defaults,
        custom,
    };
    keymap.validate()?;
    Ok(keymap)
}

/// Load and validate a strict-data `.mix` keymap from disk.
pub fn load_keymap(path: &Path) -> Result<Keymap, KeymapError> {
    let file = std::fs::File::open(path).map_err(|error| KeymapError::Read {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    let source = match read_capped_source(file) {
        Ok(source) => source,
        Err(CappedReadError::TooLarge) => {
            return Err(KeymapError::FileTooLarge {
                bytes: MAX_KEYMAP_FILE_BYTES + 1,
                maximum: MAX_KEYMAP_FILE_BYTES,
            });
        }
        Err(CappedReadError::Io(error)) => {
            return Err(KeymapError::Read {
                path: path.to_path_buf(),
                message: error.to_string(),
            });
        }
    };
    match parse_keymap(&source) {
        Err(KeymapError::Decode(message)) => Err(KeymapError::Parse {
            path: path.to_path_buf(),
            message,
        }),
        result => result,
    }
}

/// Parse and validate a strict-data `.mix` keymap string.
pub fn parse_keymap(source: &str) -> Result<Keymap, KeymapError> {
    if source.len() > MAX_KEYMAP_FILE_BYTES {
        return Err(KeymapError::FileTooLarge {
            bytes: source.len(),
            maximum: MAX_KEYMAP_FILE_BYTES,
        });
    }
    let envelope: VersionEnvelope = cosmix_config::from_conf_mix_str(source)
        .map_err(|error| KeymapError::Decode(error.to_string()))?;
    if envelope.version != KEYMAP_SCHEMA_VERSION {
        return Err(KeymapError::UnsupportedVersion(envelope.version));
    }
    let raw: RawKeymap = cosmix_config::from_conf_mix_str(source)
        .map_err(|error| KeymapError::Decode(error.to_string()))?;
    keymap_from_raw(raw)
}

pub(crate) fn read_capped_source<R: Read>(reader: R) -> Result<String, CappedReadError> {
    let mut source = Vec::with_capacity(MAX_KEYMAP_FILE_BYTES.min(8 * 1_024));
    reader
        .take((MAX_KEYMAP_FILE_BYTES + 1) as u64)
        .read_to_end(&mut source)
        .map_err(CappedReadError::Io)?;
    if source.len() > MAX_KEYMAP_FILE_BYTES {
        return Err(CappedReadError::TooLarge);
    }
    String::from_utf8(source).map_err(|error| {
        CappedReadError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    })
}

#[derive(Debug)]
pub(crate) enum CappedReadError {
    TooLarge,
    Io(std::io::Error),
}

/// Serialise a validated keymap to strict-data `.mix`.
pub fn to_keymap_mix(keymap: &Keymap) -> Result<String, KeymapError> {
    keymap.validate()?;
    let source = cosmix_config::to_conf_mix_string(keymap)
        .map_err(|error| KeymapError::Serialise(error.to_string()))?;
    if source.len() > MAX_KEYMAP_FILE_BYTES {
        return Err(KeymapError::FileTooLarge {
            bytes: source.len(),
            maximum: MAX_KEYMAP_FILE_BYTES,
        });
    }
    Ok(source)
}

/// Save a keymap as strict-data `.mix` after validation.
///
/// The format conversion is the mandated `cosmix_config`/`cosmix_mix` serde
/// path. Atomic persistence policy belongs to the app's config layer, which
/// may combine this write with revision and hot-reload bookkeeping.
pub fn save_keymap(path: &Path, keymap: &Keymap) -> Result<(), KeymapError> {
    let source = to_keymap_mix(keymap)?;
    std::fs::write(path, source).map_err(|error| KeymapError::Write(error.to_string()))
}

/// Error loading, validating or storing a keymap.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum KeymapError {
    /// A path could not be read.
    #[error("reading keymap {}: {message}", path.display())]
    Read {
        /// Attempted path.
        path: std::path::PathBuf,
        /// I/O failure.
        message: String,
    },
    /// A present file could not be decoded as a keymap.
    #[error("parsing keymap {}: {message}", path.display())]
    Parse {
        /// Attempted path.
        path: std::path::PathBuf,
        /// Decode or validation failure.
        message: String,
    },
    /// Strict-data or serde decoding failed for an in-memory source.
    #[error("decoding keymap: {0}")]
    Decode(String),
    /// The schema version is not supported.
    #[error("unsupported keymap schema version {0}")]
    UnsupportedVersion(u32),
    /// The timeout was zero or unreasonably large.
    #[error("chord_timeout_ms must be between 1 and 60000, got {0}")]
    InvalidTimeout(u64),
    /// A programmatic binding violated chord or scope invariants.
    #[error("binding for {action} is invalid: {reason}")]
    InvalidBinding {
        /// Affected action.
        action: ActionId,
        /// Invariant failure.
        reason: String,
    },
    /// A raw binding failed before its action id was interned.
    #[error("invalid keymap binding: {0}")]
    InvalidRawBinding(String),
    /// An action id failed grammar, length or interner-capacity checks.
    #[error("invalid keymap action id: {0}")]
    InvalidActionId(String),
    /// A keymap exceeded [`MAX_KEYMAP_ACTION_IDS`].
    #[error("keymap has {count} distinct action ids; maximum is {maximum}")]
    TooManyActionIds {
        /// Actual distinct count.
        count: usize,
        /// Configured maximum.
        maximum: usize,
    },
    /// A keymap exceeded [`MAX_KEYMAP_BINDINGS`].
    #[error("keymap has {count} bindings; maximum is {maximum}")]
    TooManyBindings {
        /// Actual default plus custom entry count.
        count: usize,
        /// Configured maximum.
        maximum: usize,
    },
    /// A source exceeded [`MAX_KEYMAP_FILE_BYTES`].
    #[error("keymap reached at least {bytes} bytes; maximum is {maximum}")]
    FileTooLarge {
        /// Bytes observed before rejecting the source.
        bytes: usize,
        /// Configured maximum.
        maximum: usize,
    },
    /// Strict-data serialisation failed.
    #[error("serialising keymap: {0}")]
    Serialise(String),
    /// A path could not be written.
    #[error("writing keymap: {0}")]
    Write(String),
}
