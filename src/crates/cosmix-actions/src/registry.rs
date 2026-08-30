use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize};

use crate::ActionId;

/// Maximum metadata records accepted in one transactional decode.
pub const MAX_ACTION_METADATA_ITEMS: usize = 4_096;

/// Maximum UTF-8 bytes accepted by [`parse_action_metadata`].
pub const MAX_ACTION_METADATA_BYTES: usize = 1_048_576;

/// Maximum argument fields accepted for one action.
pub const MAX_ACTION_ARGUMENT_FIELDS: usize = 128;

/// Maximum actions retained by one live registry.
pub const MAX_ACTION_REGISTRY_ITEMS: usize = MAX_ACTION_METADATA_ITEMS;

/// Maximum aggregate metadata bytes retained by one live registry.
///
/// The accounting includes ids and every owned metadata/schema string. It is
/// deliberately conservative rather than a wire-encoding byte count.
pub const MAX_ACTION_REGISTRY_BYTES: usize = MAX_ACTION_METADATA_BYTES;

/// Scalar argument types understood by the action registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionArgKind {
    /// UTF-8 string.
    String,
    /// Finite number.
    Number,
    /// Boolean.
    Boolean,
}

/// One named field in an action's argument schema.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionArg {
    /// Stable argument name.
    pub name: String,
    /// Required scalar type.
    pub kind: ActionArgKind,
    /// Whether callers must provide this argument.
    #[serde(default)]
    pub required: bool,
    /// Agent-facing explanation of the field.
    #[serde(default)]
    pub description: Option<String>,
}

/// Serializable argument contract for one action.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArgsSchema {
    /// Named accepted fields.
    #[serde(default)]
    pub fields: Vec<ActionArg>,
    /// Whether fields not listed above are accepted.
    #[serde(default)]
    pub allow_extra: bool,
}

/// Metadata for an action whose normal UI path requires local interaction.
///
/// Bus callers never open local UI. They are directed to the typed,
/// non-interactive verb when one exists, otherwise the action is local-only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractiveAction {
    /// Typed Bus verb which accepts the path or other explicit target. `None`
    /// means the action is local-only and has no non-interactive equivalent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direct_verb: Option<String>,
}

/// Invocation provenance understood by the engine-independent registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionSource {
    /// Application-owned direct invocation.
    App,
    /// Keyboard shortcut.
    Key,
    /// Pointer activation.
    Mouse,
    /// Menu activation.
    Menu,
    /// Bus ingress.
    Bus,
    /// MIDI mapping.
    Midi,
    /// OSC ingress.
    Osc,
}

/// Serializable per-action source allowlist.
///
/// The default preserves local application/UI invocation but fails closed for
/// Bus, MIDI and OSC. Remote-capable sources must be enabled explicitly for
/// each audited action. In 0.2, CTK's Bus adapter is the authoritative consumer
/// of this policy. The key, mouse and menu fields are advisory until those
/// adapters migrate to [`ActionRegistry::invoke_from`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionSources {
    /// Application-owned direct invocation.
    #[serde(default = "default_true")]
    pub app: bool,
    /// Keyboard shortcut invocation.
    #[serde(default = "default_true")]
    pub key: bool,
    /// Pointer invocation.
    #[serde(default = "default_true")]
    pub mouse: bool,
    /// Menu invocation.
    #[serde(default = "default_true")]
    pub menu: bool,
    /// Bus invocation; denied unless explicitly enabled.
    #[serde(default)]
    pub bus: bool,
    /// MIDI invocation; denied unless explicitly enabled.
    #[serde(default)]
    pub midi: bool,
    /// OSC invocation; denied unless explicitly enabled.
    #[serde(default)]
    pub osc: bool,
}

const fn default_true() -> bool {
    true
}

impl Default for ActionSources {
    fn default() -> Self {
        Self {
            app: true,
            key: true,
            mouse: true,
            menu: true,
            bus: false,
            midi: false,
            osc: false,
        }
    }
}

impl ActionSources {
    /// Local UI/application sources plus explicitly authorised Bus ingress.
    pub const BUS: Self = Self {
        app: true,
        key: true,
        mouse: true,
        menu: true,
        bus: true,
        midi: false,
        osc: false,
    };

    /// Whether this allowlist admits `source`.
    pub const fn allows(self, source: ActionSource) -> bool {
        match source {
            ActionSource::App => self.app,
            ActionSource::Key => self.key,
            ActionSource::Mouse => self.mouse,
            ActionSource::Menu => self.menu,
            ActionSource::Bus => self.bus,
            ActionSource::Midi => self.midi,
            ActionSource::Osc => self.osc,
        }
    }
}

/// Serializable, queryable metadata for one action.
///
/// This is the only registry data intended for future `actions.*` props.  It
/// contains no process pointers or closures.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ActionMeta {
    /// Stable action identifier shared by menus, keymaps and invocation.
    pub id: ActionId,
    /// Human-readable menu or command-palette label.
    pub label: String,
    /// Typed ingress argument contract.
    #[serde(default)]
    pub args_schema: ArgsSchema,
    /// Optional grouping such as `file`, `view`, or `transport`.
    #[serde(default)]
    pub category: Option<String>,
    /// Optional stable icon-system name, not rendered pixels.
    #[serde(default)]
    pub icon_name: Option<String>,
    /// Agent-facing explanation of the action's effect.
    #[serde(default)]
    pub description: Option<String>,
    /// Present when invoking this action would require a local requester or
    /// other interactive choice. Bus ingress rejects it, naming a direct verb
    /// when one exists, instead of opening UI on the operator's desktop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interactive: Option<InteractiveAction>,
    /// Invocation sources permitted to reach this action. Bus is fail-closed
    /// unless explicitly enabled.
    #[serde(default)]
    pub allowed_sources: ActionSources,
}

/// A transactionally decoded collection of serialisable action metadata.
///
/// Deserialisation first decodes all ids as owned strings, validates the whole
/// collection, and only then batch-interns them. A malformed later record can
/// therefore never leave earlier process-lifetime ids behind. Unknown
/// metadata fields are ignored for forward-compatible query consumers.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct ActionMetadata(Vec<ActionMeta>);

impl ActionMetadata {
    /// Validate and wrap programmatically constructed metadata.
    pub fn new(items: Vec<ActionMeta>) -> Result<Self, ActionMetadataError> {
        if items.len() > MAX_ACTION_METADATA_ITEMS {
            return Err(ActionMetadataError::TooManyItems {
                count: items.len(),
                maximum: MAX_ACTION_METADATA_ITEMS,
            });
        }
        for meta in &items {
            validate_schema(meta)
                .map_err(|error| ActionMetadataError::Invalid(error.to_string()))?;
        }
        Ok(Self(items))
    }

    /// Borrow metadata records in source order.
    pub fn as_slice(&self) -> &[ActionMeta] {
        &self.0
    }

    /// Iterate metadata records in source order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &ActionMeta> {
        self.0.iter()
    }

    /// Consume the collection.
    pub fn into_vec(self) -> Vec<ActionMeta> {
        self.0
    }
}

#[derive(Deserialize)]
struct RawActionMeta {
    id: String,
    label: String,
    #[serde(default)]
    args_schema: ArgsSchema,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    icon_name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    interactive: Option<InteractiveAction>,
    #[serde(default)]
    allowed_sources: ActionSources,
}

impl<'de> Deserialize<'de> for ActionMetadata {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = Vec::<RawActionMeta>::deserialize(deserializer)?;
        action_metadata_from_raw(raw).map_err(serde::de::Error::custom)
    }
}

fn action_metadata_from_raw(
    raw: Vec<RawActionMeta>,
) -> Result<ActionMetadata, ActionMetadataError> {
    if raw.len() > MAX_ACTION_METADATA_ITEMS {
        return Err(ActionMetadataError::TooManyItems {
            count: raw.len(),
            maximum: MAX_ACTION_METADATA_ITEMS,
        });
    }
    let ids: Vec<_> = raw.iter().map(|meta| meta.id.as_str()).collect();
    for id in &ids {
        ActionId::validate_str(id)
            .map_err(|error| ActionMetadataError::InvalidActionId(error.to_string()))?;
    }
    for meta in &raw {
        validate_raw_schema(meta)?;
    }
    let interned = ActionId::intern_many(&ids)
        .map_err(|error| ActionMetadataError::InvalidActionId(error.to_string()))?;
    drop(ids);
    let items = raw
        .into_iter()
        .zip(interned)
        .map(|(meta, id)| ActionMeta {
            id,
            label: meta.label,
            args_schema: meta.args_schema,
            category: meta.category,
            icon_name: meta.icon_name,
            description: meta.description,
            interactive: meta.interactive,
            allowed_sources: meta.allowed_sources,
        })
        .collect();
    Ok(ActionMetadata(items))
}

fn validate_raw_schema(meta: &RawActionMeta) -> Result<(), ActionMetadataError> {
    if meta.args_schema.fields.len() > MAX_ACTION_ARGUMENT_FIELDS {
        return Err(ActionMetadataError::TooManyArguments {
            id: meta.id.clone(),
            count: meta.args_schema.fields.len(),
            maximum: MAX_ACTION_ARGUMENT_FIELDS,
        });
    }
    let mut names = BTreeSet::new();
    for field in &meta.args_schema.fields {
        if field.name.is_empty() || !names.insert(field.name.clone()) {
            return Err(ActionMetadataError::InvalidSchema {
                id: meta.id.clone(),
                field: field.name.clone(),
            });
        }
    }
    if meta.interactive.as_ref().is_some_and(|interactive| {
        interactive
            .direct_verb
            .as_deref()
            .is_some_and(|verb| !valid_direct_verb(verb))
    }) {
        return Err(ActionMetadataError::InvalidInteractiveDirectVerb {
            id: meta.id.clone(),
        });
    }
    if meta.interactive.is_some() && meta.allowed_sources.bus {
        return Err(ActionMetadataError::InteractiveBusAllowed {
            id: meta.id.clone(),
        });
    }
    Ok(())
}

/// Parse an action metadata collection through the strict-data `.mix` bridge.
pub fn parse_action_metadata(source: &str) -> Result<ActionMetadata, ActionMetadataError> {
    if source.len() > MAX_ACTION_METADATA_BYTES {
        return Err(ActionMetadataError::SourceTooLarge {
            bytes: source.len(),
            maximum: MAX_ACTION_METADATA_BYTES,
        });
    }
    let raw: Vec<RawActionMeta> = cosmix_config::from_conf_mix_str(source)
        .map_err(|error| ActionMetadataError::Decode(error.to_string()))?;
    action_metadata_from_raw(raw)
}

/// Serialise a validated metadata collection through strict-data `.mix`.
pub fn to_action_metadata_mix(metadata: &ActionMetadata) -> Result<String, ActionMetadataError> {
    let source = cosmix_config::to_conf_mix_string(metadata)
        .map_err(|error| ActionMetadataError::Serialise(error.to_string()))?;
    if source.len() > MAX_ACTION_METADATA_BYTES {
        return Err(ActionMetadataError::SourceTooLarge {
            bytes: source.len(),
            maximum: MAX_ACTION_METADATA_BYTES,
        });
    }
    Ok(source)
}

/// Metadata collection decode or validation error.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ActionMetadataError {
    /// Strict-data or serde decoding failed.
    #[error("decoding action metadata: {0}")]
    Decode(String),
    /// A decoded id failed its grammar, length, or interner ceiling.
    #[error("invalid action metadata id: {0}")]
    InvalidActionId(String),
    /// An argument schema used an empty or duplicate field name.
    #[error("action {id} has invalid argument field {field:?}")]
    InvalidSchema {
        /// Raw action id, not yet interned.
        id: String,
        /// Empty or duplicate field.
        field: String,
    },
    /// Interactive metadata supplied a malformed typed direct verb.
    #[error("interactive action {id} has an invalid direct Bus verb")]
    InvalidInteractiveDirectVerb {
        /// Raw action id, not yet interned.
        id: String,
    },
    /// Interactive actions are absolutely prohibited from Bus invocation.
    #[error("interactive action {id} cannot allow Bus invocation")]
    InteractiveBusAllowed {
        /// Raw action id, not yet interned.
        id: String,
    },
    /// An action declared more schema fields than the decode bound.
    #[error("action {id} has {count} argument fields; maximum is {maximum}")]
    TooManyArguments {
        /// Raw action id, not yet interned.
        id: String,
        /// Actual field count.
        count: usize,
        /// Configured maximum.
        maximum: usize,
    },
    /// Programmatic metadata validation failed.
    #[error("invalid action metadata: {0}")]
    Invalid(String),
    /// A collection exceeded [`MAX_ACTION_METADATA_ITEMS`].
    #[error("action metadata has {count} items; maximum is {maximum}")]
    TooManyItems {
        /// Actual record count.
        count: usize,
        /// Configured maximum.
        maximum: usize,
    },
    /// A source exceeded [`MAX_ACTION_METADATA_BYTES`].
    #[error("action metadata is {bytes} bytes; maximum is {maximum}")]
    SourceTooLarge {
        /// Actual source size.
        bytes: usize,
        /// Configured maximum.
        maximum: usize,
    },
    /// Strict-data serialisation failed.
    #[error("serialising action metadata: {0}")]
    Serialise(String),
}

/// Scalar value supplied to an action handler.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ActionValue {
    /// Boolean value.
    Boolean(bool),
    /// Numeric value.
    Number(f64),
    /// String value.
    String(String),
}

/// App-local argument bag passed to an [`ActionHandler`].
pub type ActionArgs = BTreeMap<String, ActionValue>;

/// Return type of an app-local handler.
pub type ActionHandlerResult = Result<(), String>;

/// App-local side-effect function for an action.
///
/// It is intentionally absent from [`ActionMeta`] and never serialised.
pub type ActionHandler = Arc<dyn Fn(&ActionArgs) -> ActionHandlerResult + Send + Sync + 'static>;

/// App-local enabled-state predicate evaluated at invocation time.
///
/// The closure may capture app-owned synchronisation primitives; the registry
/// exposes only the resulting boolean, never the closure itself.
pub type EnabledPredicate = Arc<dyn Fn() -> bool + Send + Sync + 'static>;

struct RegisteredAction {
    meta: ActionMeta,
    handler: ActionHandler,
    enabled: EnabledPredicate,
}

/// App-local registry joining queryable metadata to runtime behaviour.
#[derive(Default)]
pub struct ActionRegistry {
    entries: BTreeMap<ActionId, RegisteredAction>,
    metadata_bytes: usize,
    revision: u64,
}

impl ActionRegistry {
    /// Construct an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one action, rejecting duplicate ids and malformed schemas.
    pub fn register(
        &mut self,
        meta: ActionMeta,
        handler: ActionHandler,
        enabled: EnabledPredicate,
    ) -> Result<(), RegistryError> {
        validate_schema(&meta)?;
        if self.entries.contains_key(&meta.id) {
            return Err(RegistryError::Duplicate(meta.id));
        }
        if self.entries.len() >= MAX_ACTION_REGISTRY_ITEMS {
            return Err(RegistryError::RegistryItemLimit {
                maximum: MAX_ACTION_REGISTRY_ITEMS,
            });
        }
        let added_bytes = metadata_size(&meta);
        let metadata_bytes = self.metadata_bytes.saturating_add(added_bytes);
        if metadata_bytes > MAX_ACTION_REGISTRY_BYTES {
            return Err(RegistryError::RegistryMetadataLimit {
                bytes: metadata_bytes,
                maximum: MAX_ACTION_REGISTRY_BYTES,
            });
        }
        self.entries.insert(
            meta.id,
            RegisteredAction {
                meta,
                handler,
                enabled,
            },
        );
        self.metadata_bytes = metadata_bytes;
        self.revision = self.revision.saturating_add(1);
        Ok(())
    }

    /// Return serialisable metadata for an id.
    pub fn metadata(&self, id: ActionId) -> Option<&ActionMeta> {
        self.entries.get(&id).map(|entry| &entry.meta)
    }

    /// Return serialisable metadata by an untrusted runtime name without
    /// interning it. Bus and similar ingress should use this before converting
    /// a caller-supplied id into the registry's already-interned [`ActionId`].
    pub fn metadata_named(&self, id: &str) -> Option<&ActionMeta> {
        self.entries.get(id).map(|entry| &entry.meta)
    }

    /// Iterate serialisable metadata in stable id order.
    pub fn iter_metadata(&self) -> impl ExactSizeIterator<Item = &ActionMeta> {
        self.entries.values().map(|entry| &entry.meta)
    }

    /// Clone the queryable registry surface for publication or inspection.
    pub fn metadata_snapshot(&self) -> Vec<ActionMeta> {
        self.iter_metadata().cloned().collect()
    }

    /// Clone the queryable registry surface as a transactional collection.
    pub fn metadata_collection(&self) -> ActionMetadata {
        ActionMetadata(self.metadata_snapshot())
    }

    /// Whether an action's runtime predicate currently permits invocation.
    pub fn is_enabled(&self, id: ActionId) -> Option<bool> {
        self.entries.get(&id).map(|entry| (entry.enabled)())
    }

    /// Validate arguments and invoke one enabled app-local handler.
    pub fn invoke(&self, id: ActionId, args: &ActionArgs) -> Result<(), RegistryError> {
        let entry = self.validated_entry(id, args, ActionSource::App)?;
        (entry.handler)(args).map_err(|message| RegistryError::Handler { id, message })
    }

    /// Validate source and arguments, then invoke one enabled handler.
    pub fn invoke_from(
        &self,
        id: ActionId,
        args: &ActionArgs,
        source: ActionSource,
    ) -> Result<(), RegistryError> {
        let entry = self.validated_entry(id, args, source)?;
        (entry.handler)(args).map_err(|message| RegistryError::Handler { id, message })
    }

    /// Re-check registration, the live enabled predicate, and typed arguments
    /// without running the app-local handler.
    ///
    /// Event-bus adapters use this immediately before publishing their own
    /// action request, keeping validation authoritative without duplicating the
    /// eventual consumer's side effects.
    pub fn validate_invocation(
        &self,
        id: ActionId,
        args: &ActionArgs,
    ) -> Result<&ActionMeta, RegistryError> {
        Ok(&self.validated_entry(id, args, ActionSource::App)?.meta)
    }

    /// Re-check registration, source policy, live enabled state and arguments
    /// without running the handler.
    pub fn validate_invocation_from(
        &self,
        id: ActionId,
        args: &ActionArgs,
        source: ActionSource,
    ) -> Result<&ActionMeta, RegistryError> {
        Ok(&self.validated_entry(id, args, source)?.meta)
    }

    fn validated_entry(
        &self,
        id: ActionId,
        args: &ActionArgs,
        source: ActionSource,
    ) -> Result<&RegisteredAction, RegistryError> {
        let entry = self.entries.get(&id).ok_or(RegistryError::Unknown(id))?;
        if !(entry.enabled)() {
            return Err(RegistryError::Disabled(id));
        }
        validate_args(&entry.meta, args)?;
        if !entry.meta.allowed_sources.allows(source) {
            return Err(RegistryError::SourceNotAllowed {
                id,
                invocation_source: source,
            });
        }
        Ok(entry)
    }

    /// Number of registered actions.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the registry has no actions.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Monotonic structural revision, advanced after each successful register.
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Aggregate owned metadata bytes charged against the registry bound.
    pub const fn metadata_bytes(&self) -> usize {
        self.metadata_bytes
    }
}

fn validate_schema(meta: &ActionMeta) -> Result<(), RegistryError> {
    if meta.args_schema.fields.len() > MAX_ACTION_ARGUMENT_FIELDS {
        return Err(RegistryError::TooManyArguments {
            id: meta.id,
            count: meta.args_schema.fields.len(),
            maximum: MAX_ACTION_ARGUMENT_FIELDS,
        });
    }
    let mut names = BTreeSet::new();
    for field in &meta.args_schema.fields {
        if field.name.is_empty() || !names.insert(field.name.clone()) {
            return Err(RegistryError::InvalidSchema {
                id: meta.id,
                field: field.name.clone(),
            });
        }
    }
    if meta.interactive.as_ref().is_some_and(|interactive| {
        interactive
            .direct_verb
            .as_deref()
            .is_some_and(|verb| !valid_direct_verb(verb))
    }) {
        return Err(RegistryError::InvalidInteractiveDirectVerb { id: meta.id });
    }
    if meta.interactive.is_some() && meta.allowed_sources.bus {
        return Err(RegistryError::InteractiveBusAllowed { id: meta.id });
    }
    Ok(())
}

fn metadata_size(meta: &ActionMeta) -> usize {
    let optional = |value: &Option<String>| value.as_ref().map_or(0, String::len);
    meta.id
        .as_str()
        .len()
        .saturating_add(meta.label.len())
        .saturating_add(optional(&meta.category))
        .saturating_add(optional(&meta.icon_name))
        .saturating_add(optional(&meta.description))
        .saturating_add(
            meta.interactive
                .as_ref()
                .and_then(|value| value.direct_verb.as_ref())
                .map_or(0, String::len),
        )
        .saturating_add(meta.args_schema.fields.iter().fold(0usize, |total, field| {
            total
                .saturating_add(field.name.len())
                .saturating_add(optional(&field.description))
        }))
}

fn valid_direct_verb(verb: &str) -> bool {
    verb.len() <= 128
        && verb.starts_with("app.")
        && verb.split('.').all(|part| {
            !part.is_empty()
                && part.split('-').all(|segment| {
                    let mut bytes = segment.bytes();
                    bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
                        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
                })
        })
}

fn validate_args(meta: &ActionMeta, args: &ActionArgs) -> Result<(), RegistryError> {
    for field in &meta.args_schema.fields {
        let Some(value) = args.get(&field.name) else {
            if field.required {
                return Err(RegistryError::MissingArgument {
                    id: meta.id,
                    argument: field.name.clone(),
                });
            }
            continue;
        };
        let valid = match (field.kind, value) {
            (ActionArgKind::String, ActionValue::String(_))
            | (ActionArgKind::Boolean, ActionValue::Boolean(_)) => true,
            (ActionArgKind::Number, ActionValue::Number(number)) => number.is_finite(),
            _ => false,
        };
        if !valid {
            return Err(RegistryError::WrongArgumentType {
                id: meta.id,
                argument: field.name.clone(),
                expected: field.kind,
            });
        }
    }
    if !meta.args_schema.allow_extra {
        let known: BTreeSet<_> = meta
            .args_schema
            .fields
            .iter()
            .map(|field| field.name.as_str())
            .collect();
        if let Some(extra) = args.keys().find(|name| !known.contains(name.as_str())) {
            return Err(RegistryError::UnexpectedArgument {
                id: meta.id,
                argument: extra.clone(),
            });
        }
    }
    Ok(())
}

/// Registry definition, state or invocation error.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    /// The id is already registered.
    #[error("action {0} is already registered")]
    Duplicate(ActionId),
    /// The id is not registered.
    #[error("unknown action {0}")]
    Unknown(ActionId),
    /// The enabled predicate rejected invocation.
    #[error("action {0} is disabled")]
    Disabled(ActionId),
    /// An argument schema used an empty or duplicate field name.
    #[error("action {id} has invalid argument field {field:?}")]
    InvalidSchema {
        /// Affected action.
        id: ActionId,
        /// Empty or duplicate field.
        field: String,
    },
    /// Interactive metadata supplied a malformed typed direct verb.
    #[error("interactive action {id} has an invalid direct Bus verb")]
    InvalidInteractiveDirectVerb {
        /// Affected action.
        id: ActionId,
    },
    /// An interactive action attempted to allow Bus invocation.
    #[error("interactive action {id} cannot allow Bus invocation")]
    InteractiveBusAllowed {
        /// Affected action.
        id: ActionId,
    },
    /// An action declared more schema fields than the runtime bound.
    #[error("action {id} has {count} argument fields; maximum is {maximum}")]
    TooManyArguments {
        /// Affected action.
        id: ActionId,
        /// Actual field count.
        count: usize,
        /// Configured maximum.
        maximum: usize,
    },
    /// The source is not in this action's explicit allowlist.
    #[error("action {id} does not permit {invocation_source:?} invocation")]
    SourceNotAllowed {
        /// Affected action.
        id: ActionId,
        /// Rejected source.
        invocation_source: ActionSource,
    },
    /// The live registry reached its item ceiling.
    #[error("action registry item limit reached; maximum is {maximum}")]
    RegistryItemLimit {
        /// Configured maximum.
        maximum: usize,
    },
    /// The live registry exceeded its aggregate metadata ceiling.
    #[error("action registry metadata is {bytes} bytes; maximum is {maximum}")]
    RegistryMetadataLimit {
        /// Prospective aggregate bytes.
        bytes: usize,
        /// Configured maximum.
        maximum: usize,
    },
    /// A required argument was absent.
    #[error("action {id} requires argument {argument:?}")]
    MissingArgument {
        /// Affected action.
        id: ActionId,
        /// Missing argument.
        argument: String,
    },
    /// An argument had the wrong scalar type.
    #[error("action {id} argument {argument:?} must be {expected:?}")]
    WrongArgumentType {
        /// Affected action.
        id: ActionId,
        /// Invalid argument.
        argument: String,
        /// Required type.
        expected: ActionArgKind,
    },
    /// An undeclared argument was supplied to a closed schema.
    #[error("action {id} does not accept argument {argument:?}")]
    UnexpectedArgument {
        /// Affected action.
        id: ActionId,
        /// Undeclared argument.
        argument: String,
    },
    /// The app-local handler returned a failure.
    #[error("action {id} failed: {message}")]
    Handler {
        /// Affected action.
        id: ActionId,
        /// App-provided failure text.
        message: String,
    },
}
