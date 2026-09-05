//! Namespace declarations per SPEC 12 §6.
//!
//! A namespace is the unit a daemon registers with the property substrate:
//! a name (unqualified, per-service — e.g. `accounts`), a schema, a
//! cardinality (singleton vs collection, carrying the canonical key /
//! primary key field), a per-record lifecycle mode (`Simple` vs `Saga`),
//! a chosen storage backend, an external-edit policy, an `AuthPolicy`,
//! and the hook handler.
//!
//! The wire `namespace:` header carries the **unqualified** name
//! (`accounts`). The fully-qualified form (`<svc>.<name>`, e.g.
//! `maild.accounts`) is library-derived from the registration's owning
//! service and only appears in capability strings (`props.write:maild.accounts`)
//! and audit/event verb strings (`maild.props.set`). Per SPEC 12 §4.1 the
//! two forms are distinct concepts and the substrate keeps them separate.
//!
//! C1 ships the type surface: all 16 fields of §6.1 are present so
//! storage, dispatcher, Bus, and audit layers built in C2..C6 land
//! against a stable shape. Field bodies for schema-language details
//! (`PropertySchema`, `FieldType`, `Validator`) are placeholder shapes
//! whose richer semantics arrive with the validators wiring.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::capability::CapabilitySet;
use crate::hooks::{HookError, Hooks};
use cosmix_props_core::value::PropValue;

/// Unqualified namespace name, e.g. `accounts`. Matches the SPEC 12 §4.1
/// regex `[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*` — single segment OK, dots
/// permitted between lowercase identifiers (`theme.dark` is one name).
///
/// The fully-qualified form `<service>.<name>` is constructed via
/// [`NamespaceName::qualified`] and is NOT a separate type; it is only
/// used as a string in capability vocabularies and event verbs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
// INVARIANT: every value originates from `new` (FromStr and Deserialize
// both route through it); Serialize stays derived-transparent on that
// basis. No other constructor may set this field.
pub struct NamespaceName(String);

// Manual impl so wire input is validated by `new` — the derived
// transparent form would admit any string.
impl<'de> Deserialize<'de> for NamespaceName {
    fn deserialize<D>(de: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(de)?;
        Self::new(s).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamespaceNameError {
    Empty,
    EmptySegment,
    InvalidLeadingChar { segment: String, ch: char },
    InvalidChar { segment: String, ch: char },
}

impl fmt::Display for NamespaceNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "namespace name is empty"),
            Self::EmptySegment => write!(f, "namespace name has empty segment"),
            Self::InvalidLeadingChar { segment, ch } => write!(
                f,
                "segment {segment:?} starts with invalid char {ch:?}; must be [a-z]"
            ),
            Self::InvalidChar { segment, ch } => {
                write!(f, "segment {segment:?} contains invalid char {ch:?}")
            }
        }
    }
}

impl std::error::Error for NamespaceNameError {}

impl NamespaceName {
    pub fn new(s: impl Into<String>) -> Result<Self, NamespaceNameError> {
        let s = s.into();
        if s.is_empty() {
            return Err(NamespaceNameError::Empty);
        }
        for seg in s.split('.') {
            if seg.is_empty() {
                return Err(NamespaceNameError::EmptySegment);
            }
            let mut chars = seg.chars();
            let first = chars.next().expect("non-empty segment");
            if !first.is_ascii_lowercase() {
                return Err(NamespaceNameError::InvalidLeadingChar {
                    segment: seg.to_string(),
                    ch: first,
                });
            }
            for ch in chars {
                if !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_') {
                    return Err(NamespaceNameError::InvalidChar {
                        segment: seg.to_string(),
                        ch,
                    });
                }
            }
        }
        Ok(Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Compose the fully qualified `<service>.<name>` form. Used in
    /// capability strings and audit/event verbs; never on the
    /// `namespace:` wire header (which carries `self.as_str()`).
    pub fn qualified(&self, service: &str) -> String {
        format!("{}.{}", service, self.0)
    }
}

impl fmt::Display for NamespaceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::str::FromStr for NamespaceName {
    type Err = NamespaceNameError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

/// SPEC 12 §6.2 — cardinality declaration carrying the key-shape data
/// the substrate needs at dispatch time.
///
/// - `Singleton { canonical_key }`: at most one record. The wire `key:`
///   header is the empty string `""`; the substrate fills it in on
///   responses with `canonical_key`.
/// - `Collection { primary_key_field }`: zero or more records keyed by
///   the named schema field. The substrate enforces uniqueness on that
///   field and exposes its value as the wire `key:` in responses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Cardinality {
    Singleton { canonical_key: String },
    Collection { primary_key_field: String },
}

impl Cardinality {
    pub fn is_singleton(&self) -> bool {
        matches!(self, Self::Singleton { .. })
    }

    pub fn is_collection(&self) -> bool {
        matches!(self, Self::Collection { .. })
    }
}

/// SPEC 12 §6.5 — Simple namespaces are plain CRUD; Saga namespaces
/// carry the substrate-owned `_lifecycle` field and drive the
/// `Provisioning → Active|Failed` transition through `after_set`.
///
/// Spec name: `NamespaceLifecycle` (§6.1). The pre-rename in-tree alias
/// was `LifecycleMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NamespaceLifecycle {
    #[default]
    Simple,
    Saga,
}

/// SPEC 12 §6.4 — pick exactly one backend per namespace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StorageBackendKind {
    Memory,
    MixData,
    Toml,
    SqliteTable {
        /// Existing table in the daemon's SQLite database that backs this
        /// namespace's records. Substrate also creates a sibling
        /// event-history table; see `cosmix_props::store`.
        table: String,
    },
}

/// SPEC 12 §5.4 — soft vs hard delete behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeleteMode {
    /// Tombstone retained for `tombstone_ttl`; `if_version` works across
    /// delete-then-recreate. Default.
    #[default]
    SoftDelete,
    /// Storage row removed immediately. Version sequence restarts at 1
    /// on recreate. Incompatible with `require_version` namespaces.
    HardDelete,
}

/// SPEC 12 §5.5 / §6.6 — bound on retained event-history entries used
/// for `since_nseq` replay and dispatcher recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayWindow {
    pub max_entries: u32,
    pub max_age: Duration,
}

impl Default for ReplayWindow {
    fn default() -> Self {
        Self {
            max_entries: 1024,
            max_age: Duration::from_secs(3600),
        }
    }
}

/// SPEC 12 §5.5 — body shape for `<svc>.props.records.changed` events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscribePayload {
    /// Empty event body; subscribers call `<svc>.props.get` for the new
    /// value. REQUIRED whenever any schema field is `secret`. Default.
    #[default]
    Pointer,
    /// Event body carries the full new record inline. Permitted only
    /// when no field is `secret`.
    Full,
}

/// SPEC 12 §5.6 — declarative deny-switch for the redacted public
/// describe view. **Does not grant capabilities** — the `AuthPolicy`
/// is the single source of truth for who holds
/// `props.describe:<svc>.<ns>:public`. This setting only governs
/// whether `view: public` is reachable *at all* on the namespace:
///
/// - `Allow` (default) — `view: public` is reachable for callers
///   the `AuthPolicy` grants `props.describe:<svc>.<ns>:public` to.
///   Without the capability, the verb returns `auth_denied` as
///   normal. This is *not* an "anyone can read" switch.
/// - `Deny` — `view: public` returns `auth_denied` regardless of
///   capability, even if the `AuthPolicy` would otherwise allow it.
///   Appropriate for namespaces whose mere field-name surface is
///   sensitive (e.g. peer-list shape on a node that doesn't wish to
///   advertise it). See §5.6 final paragraph.
///
/// Namespaces that want a genuinely-public describe surface declare
/// so by granting `props.describe:<svc>.<ns>:public` in the
/// `AuthPolicy` to unauthenticated peers — the cap grant, not this
/// enum, is the public-visibility decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchemaPublic {
    /// `view: public` is reachable subject to the `AuthPolicy`
    /// granting `props.describe:<svc>.<ns>:public`. Default.
    #[default]
    Allow,
    /// `view: public` returns `auth_denied` unconditionally. Use when
    /// the field-name surface is itself sensitive.
    Deny,
}

/// SPEC 12 §11 — what to do when on-disk record bytes diverge from the
/// substrate's last-known sidecar hash on startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalEditPolicy {
    /// Emit synthetic `<svc>.props.reconcile`, bump `audit_epoch`,
    /// promote on-disk bytes. Default.
    #[default]
    ReconcileAndContinue,
    /// Refuse to serve until an operator runs explicit reconciliation.
    /// For namespaces whose audit continuity is load-bearing.
    RefuseStartup,
}

/// SPEC 12 §13 — substrate conformance levels. SPEC 07 owns L0..L3,
/// SPEC 12 owns L4..L5. A namespace MAY declare a level lower than its
/// host service to publish itself as intentionally read-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConformanceLevel {
    L0,
    L1,
    L2,
    L3,
    /// SPEC 12 entry level — adds `set`/`delete`/`validate`. Default.
    #[default]
    L4,
    /// Adds `audit.watch`, saga lifecycle, reconcile semantics.
    L5,
}

/// SPEC 12 §8.3 / §12 — substrate schema version this namespace was
/// introduced in. Wire shape is the dotted string `"<major>.<minor>.<patch>"`
/// per the §8.3 describe example (`"schema_version": "0.1.0"`).
///
/// In-memory ordering is component-wise on `(major, minor, patch)`,
/// not lexicographic on the wire string — `0.10.0 > 0.9.0` even though
/// the strings sort the other way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SchemaVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl SchemaVersion {
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }
}

impl Default for SchemaVersion {
    fn default() -> Self {
        Self::new(0, 1, 0)
    }
}

impl fmt::Display for SchemaVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum SchemaVersionParseError {
    WrongComponentCount,
    InvalidComponent(String),
}

impl fmt::Display for SchemaVersionParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongComponentCount => {
                write!(f, "schema version must be `major.minor.patch`")
            }
            Self::InvalidComponent(c) => {
                write!(f, "schema version component is not a u32: {c:?}")
            }
        }
    }
}

impl std::error::Error for SchemaVersionParseError {}

impl std::str::FromStr for SchemaVersion {
    type Err = SchemaVersionParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split('.').collect();
        if parts.len() != 3 {
            return Err(SchemaVersionParseError::WrongComponentCount);
        }
        let parse = |p: &str| -> Result<u32, SchemaVersionParseError> {
            p.parse()
                .map_err(|_| SchemaVersionParseError::InvalidComponent(p.to_string()))
        };
        Ok(Self::new(
            parse(parts[0])?,
            parse(parts[1])?,
            parse(parts[2])?,
        ))
    }
}

impl Serialize for SchemaVersion {
    fn serialize<S>(&self, ser: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        ser.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for SchemaVersion {
    fn deserialize<D>(de: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(de)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// SPEC 12 §7.3 — caller identity attached by the Bus transport. The
/// AuthPolicy reads this read-only and returns the matching capability
/// set. C1 ships the field surface; C4 plumbs the Bus wire extraction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeerIdentity {
    pub unix_uid: Option<u32>,
    pub unix_gid: Option<u32>,
    pub unix_groups: Vec<String>,
    /// WireGuard peer tunnel IP, when the transport sits over WG.
    pub wg_peer: Option<String>,
    /// SPEC 10 daemon-identity verified principal, when the cross-mesh
    /// handshake provided one.
    pub signed_ident: Option<String>,
    /// Broker-attached registered service name for service peers.
    pub service_name: Option<String>,
}

/// SPEC 12 §7.2 — pure mapping from peer identity to capability set.
/// Called once per Bus request, before verb dispatch.
///
/// Stored as `Arc<dyn Fn>` so policies can close over per-namespace
/// state (capability sets, in-memory trust/grants caches built from
/// substrate watches, etc.) without resorting to globals. `Clone`
/// (not `Copy`) — `NamespaceSpec` clones share the underlying
/// closure through the `Arc`. Construction is via
/// [`AuthPolicy::new`]; evaluation is via [`AuthPolicy::resolve`].
#[derive(Clone)]
pub struct AuthPolicy {
    resolve: Arc<dyn Fn(&PeerIdentity) -> CapabilitySet + Send + Sync + 'static>,
}

impl fmt::Debug for AuthPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthPolicy").finish_non_exhaustive()
    }
}

impl AuthPolicy {
    /// Wrap a resolve closure into an `AuthPolicy`. The closure runs
    /// once per Bus request before verb dispatch and MUST stay
    /// synchronous — see SPEC 12 §7.2.
    pub fn new<F>(resolve: F) -> Self
    where
        F: Fn(&PeerIdentity) -> CapabilitySet + Send + Sync + 'static,
    {
        Self {
            resolve: Arc::new(resolve),
        }
    }

    /// Evaluate the policy for the given peer identity.
    pub fn resolve(&self, peer: &PeerIdentity) -> CapabilitySet {
        (self.resolve)(peer)
    }

    /// Convenience: a policy that grants no capabilities to anyone.
    /// Useful for read-only fixtures and for namespaces that intend to
    /// be unreachable until a real policy is plugged in.
    pub fn deny_all() -> Self {
        Self::new(|_| CapabilitySet::empty())
    }
}

/// SPEC 12 §4.3 / §6.1 — narrow type system for schema fields. The
/// substrate's type vocabulary is deliberately smaller than serde's so
/// that GUIs and CLIs can render every field generically. Schema
/// derivation (`#[derive(Property)]`) is proposed for v0.2 per §6.3;
/// v0.1 daemons hand-write the schema.
///
/// **Wire shape** (SPEC 12 §8.3). Primitive variants serialise as bare
/// JSON strings (`"bool"`, `"i64"`, `"string"`, …) — that is the
/// `"type": "string"` shape the spec example commits to. Compound
/// variants (which §8.3 does not example) serialise as objects with a
/// `kind` discriminator:
///
/// ```json
/// "bool"
/// {"kind": "enum",   "variants": ["smtp", "imap"]}
/// {"kind": "list",   "item":     "string"}
/// {"kind": "map",    "value":    "i64"}
/// {"kind": "struct", "fields":   [<FieldSchema>, ...]}
/// {"kind": "option", "inner":    "email"}
/// ```
#[derive(Debug, Clone)]
pub enum FieldType {
    Bool,
    I64,
    U64,
    F64,
    String,
    Bytes,
    Path,
    Email,
    Url,
    Enum { variants: Vec<String> },
    List { item: Box<FieldType> },
    Map { value: Box<FieldType> },
    Struct { fields: Vec<FieldSchema> },
    Option { inner: Box<FieldType> },
}

impl FieldType {
    /// Spec-§8.3 primitive token, or `None` for compound variants.
    fn primitive_token(&self) -> Option<&'static str> {
        Some(match self {
            Self::Bool => "bool",
            Self::I64 => "i64",
            Self::U64 => "u64",
            Self::F64 => "f64",
            Self::String => "string",
            Self::Bytes => "bytes",
            Self::Path => "path",
            Self::Email => "email",
            Self::Url => "url",
            Self::Enum { .. }
            | Self::List { .. }
            | Self::Map { .. }
            | Self::Struct { .. }
            | Self::Option { .. } => return None,
        })
    }

    fn from_primitive_token(s: &str) -> Option<Self> {
        Some(match s {
            "bool" => Self::Bool,
            "i64" => Self::I64,
            "u64" => Self::U64,
            "f64" => Self::F64,
            "string" => Self::String,
            "bytes" => Self::Bytes,
            "path" => Self::Path,
            "email" => Self::Email,
            "url" => Self::Url,
            _ => return None,
        })
    }
}

impl Serialize for FieldType {
    fn serialize<S>(&self, ser: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;
        if let Some(tok) = self.primitive_token() {
            return ser.serialize_str(tok);
        }
        let mut m = ser.serialize_map(Some(2))?;
        match self {
            Self::Enum { variants } => {
                m.serialize_entry("kind", "enum")?;
                m.serialize_entry("variants", variants)?;
            }
            Self::List { item } => {
                m.serialize_entry("kind", "list")?;
                m.serialize_entry("item", item)?;
            }
            Self::Map { value } => {
                m.serialize_entry("kind", "map")?;
                m.serialize_entry("value", value)?;
            }
            Self::Struct { fields } => {
                m.serialize_entry("kind", "struct")?;
                m.serialize_entry("fields", fields)?;
            }
            Self::Option { inner } => {
                m.serialize_entry("kind", "option")?;
                m.serialize_entry("inner", inner)?;
            }
            _ => unreachable!("primitive handled above"),
        }
        m.end()
    }
}

impl<'de> Deserialize<'de> for FieldType {
    fn deserialize<D>(de: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Token(String),
            Compound(CompoundRepr),
        }
        #[derive(Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        enum CompoundRepr {
            Enum { variants: Vec<String> },
            List { item: Box<FieldType> },
            Map { value: Box<FieldType> },
            Struct { fields: Vec<FieldSchema> },
            Option { inner: Box<FieldType> },
        }
        match Repr::deserialize(de)? {
            Repr::Token(s) => FieldType::from_primitive_token(&s).ok_or_else(|| {
                serde::de::Error::custom(format!("unknown FieldType primitive: {s:?}"))
            }),
            Repr::Compound(c) => Ok(match c {
                CompoundRepr::Enum { variants } => FieldType::Enum { variants },
                CompoundRepr::List { item } => FieldType::List { item },
                CompoundRepr::Map { value } => FieldType::Map { value },
                CompoundRepr::Struct { fields } => FieldType::Struct { fields },
                CompoundRepr::Option { inner } => FieldType::Option { inner },
            }),
        }
    }
}

/// SPEC 12 §4.3 — one field of a namespace schema. Wire shape is the
/// §5.6 describe body; secret fields are omitted by the dispatcher
/// before serialisation when `view: public`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldSchema {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: FieldType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<PropValue>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub secret: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub help: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<SchemaVersion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<SchemaVersion>,
    /// Names of validators on the owning `NamespaceSpec` that this
    /// field requests. The dispatcher resolves these to runtime
    /// `Validator` instances when running `<svc>.props.validate` /
    /// `<svc>.props.set`. Empty when the field uses only the
    /// type-level checks above.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub validators: Vec<String>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// SPEC 12 §6.1 — the field-list portion of a namespace's record
/// schema. Hand-written in v0.1; a `#[derive(Property)]` companion is
/// proposed for v0.2 (§6.3).
///
/// **Not the §5.6 describe wire body.** §8.3's example envelope
/// (`{namespace, schema_version, cardinality, primary_key_field,
/// fields: [...]}`) is composed at describe-verb time by combining
/// these fields with the owning `NamespaceSpec`'s `name`,
/// `cardinality`, and (future) `schema_version`. The verb-level
/// projection type arrives with C4 (`<svc>.props.describe`); C1
/// commits only to the field-list shape and to the per-field wire
/// shapes (`FieldSchema`, `FieldType`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PropertySchema {
    pub fields: Vec<FieldSchema>,
}

impl PropertySchema {
    pub fn new(fields: Vec<FieldSchema>) -> Self {
        Self { fields }
    }

    /// Return the named field, if any.
    pub fn field(&self, name: &str) -> Option<&FieldSchema> {
        self.fields.iter().find(|f| f.name == name)
    }

    /// Names of secret fields. Used by the records.changed dispatcher
    /// to compute `secret_fields_changed_count` and by the audit layer
    /// to redact field-value bytes.
    pub fn secret_field_names(&self) -> impl Iterator<Item = &str> {
        self.fields
            .iter()
            .filter(|f| f.secret)
            .map(|f| f.name.as_str())
    }
}

/// Shared closure type backing [`Validator::check`]. Validator instances
/// are stored on `NamespaceSpec`, which is `Clone`; the `Arc` lets the
/// dispatcher hand the check function to multiple worker tasks without
/// reallocation.
pub type ValidatorFn = Arc<dyn Fn(&PropValue) -> Result<(), HookError> + Send + Sync>;

/// SPEC 12 §6.1 — pure validator (distinct from hooks). Runs on
/// `<svc>.props.validate` and at the start of `<svc>.props.set` before
/// `before_set`. Validators are pure, do not perform IO, and fail with
/// `HookError::Validation` to surface as `validation_error` on the wire.
///
/// **Wire projection.** The describe verb (§5.6) projects a validator
/// to `{ "name": "..." }` under `view: full`. Under `view: public` the
/// projection is `{ "name": "<redacted>" }` when `validator_secret`
/// is set — see §5.6 for the rationale (regex patterns that themselves
/// leak security surface).
#[derive(Clone)]
pub struct Validator {
    pub name: &'static str,
    /// SPEC 12 §5.6 — when true, the validator's identity (and any
    /// regex pattern it embeds) is itself sensitive and is replaced
    /// with `<redacted>` in the public describe view.
    pub validator_secret: bool,
    pub check: ValidatorFn,
}

impl Validator {
    /// Convenience constructor for the common non-secret case.
    pub fn new(name: &'static str, check: ValidatorFn) -> Self {
        Self {
            name,
            validator_secret: false,
            check,
        }
    }
}

impl fmt::Debug for Validator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Validator")
            .field("name", &self.name)
            .field("validator_secret", &self.validator_secret)
            .finish_non_exhaustive()
    }
}

/// SPEC 12 §6.1 — the value daemons register with the substrate. All
/// 16 fields are present; defaults match the spec text and let a
/// minimal `NamespaceSpec::new(name, schema, cardinality, storage)`
/// call adopt the right behavior for a new namespace.
///
/// Not `Serialize`/`Deserialize`: contains closure handles
/// (`AuthPolicy`'s `Arc<dyn Fn>`), trait objects (`Hooks`,
/// `Validator.check`), and is a registration-time value, not a
/// wire value.
#[derive(Debug, Clone)]
pub struct NamespaceSpec {
    /// Per-service unqualified name. The library composes
    /// `<service>.<name>` when needed for capabilities and audit verbs.
    pub name: NamespaceName,

    /// Record schema. See §4.3, §6.1.
    pub schema: PropertySchema,

    pub cardinality: Cardinality,
    pub storage: StorageBackendKind,

    /// Maps peer identity → capability set. See §7.
    pub auth: AuthPolicy,

    /// Side-effectful hooks fired around storage commits. See §6.5.
    pub hooks: Hooks,

    /// Pure validators. See §5.7. Run before `before_set`.
    pub validators: Vec<Validator>,

    pub delete_mode: DeleteMode,

    /// Tombstone retention for `SoftDelete` namespaces. Default: 7 days.
    pub tombstone_ttl: Duration,

    pub replay_window: ReplayWindow,
    pub subscribe_payload: SubscribePayload,
    pub schema_public: SchemaPublic,
    pub lifecycle: NamespaceLifecycle,
    pub external_edit_policy: ExternalEditPolicy,
    pub conformance_level: ConformanceLevel,
    pub since_version: SchemaVersion,

    /// SPEC 12 §4.4 — when `true`, `<svc>.props.set` and
    /// `<svc>.props.delete` MUST carry an `if_version` header; missing
    /// header is `validation_error`. Default `false` (last-write-wins
    /// is permitted, per §4.4: "the right default for human-driven
    /// edits"). Namespaces with automation writers SHOULD set this to
    /// `true`.
    pub require_version: bool,
}

impl NamespaceSpec {
    /// Convenience constructor — minimal required fields, everything
    /// else takes its spec-default value. Matches §6.1: "A namespace
    /// registered with only `name`, `schema`, `cardinality`, and
    /// `storage` adopts the substrate library defaults for every other
    /// field."
    pub fn new(
        name: NamespaceName,
        schema: PropertySchema,
        cardinality: Cardinality,
        storage: StorageBackendKind,
    ) -> Self {
        Self {
            name,
            schema,
            cardinality,
            storage,
            auth: AuthPolicy::deny_all(),
            hooks: Hooks::noop(),
            validators: Vec::new(),
            delete_mode: DeleteMode::default(),
            tombstone_ttl: Duration::from_secs(7 * 24 * 3600),
            replay_window: ReplayWindow::default(),
            subscribe_payload: SubscribePayload::default(),
            schema_public: SchemaPublic::default(),
            lifecycle: NamespaceLifecycle::default(),
            external_edit_policy: ExternalEditPolicy::default(),
            conformance_level: ConformanceLevel::default(),
            since_version: SchemaVersion::default(),
            require_version: false,
        }
    }

    pub fn is_saga(&self) -> bool {
        matches!(self.lifecycle, NamespaceLifecycle::Saga)
    }

    /// Compose the fully qualified `<service>.<name>` form for use in
    /// capability strings and audit/event verbs.
    pub fn fqn(&self, service: &str) -> String {
        self.name.qualified(service)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_schema() -> PropertySchema {
        PropertySchema::default()
    }

    #[test]
    fn name_accepts_unqualified_and_dotted() {
        for s in ["accounts", "theme", "theme_v2", "audit.entries", "a.b.c.d"] {
            assert!(NamespaceName::new(s).is_ok(), "should accept {s}");
        }
    }

    #[test]
    fn name_rejects_invalid() {
        assert_eq!(
            NamespaceName::new("").unwrap_err(),
            NamespaceNameError::Empty
        );
        assert_eq!(
            NamespaceName::new("a..b").unwrap_err(),
            NamespaceNameError::EmptySegment
        );
        assert_eq!(
            NamespaceName::new("Accounts").unwrap_err(),
            NamespaceNameError::InvalidLeadingChar {
                segment: "Accounts".into(),
                ch: 'A'
            }
        );
        assert_eq!(
            NamespaceName::new("9accounts").unwrap_err(),
            NamespaceNameError::InvalidLeadingChar {
                segment: "9accounts".into(),
                ch: '9'
            }
        );
        assert_eq!(
            NamespaceName::new("acc-ounts").unwrap_err(),
            NamespaceNameError::InvalidChar {
                segment: "acc-ounts".into(),
                ch: '-'
            }
        );
    }

    #[test]
    fn name_serde_rejects_invalid() {
        for j in ["", "a..b", "Accounts", "9accounts", "acc-ounts"] {
            assert!(
                serde_json::from_value::<NamespaceName>(serde_json::json!(j)).is_err(),
                "should reject {j:?}"
            );
        }
    }

    #[test]
    fn name_serde_round_trips_valid() {
        let n: NamespaceName = serde_json::from_value(serde_json::json!("audit.entries")).unwrap();
        assert_eq!(n.as_str(), "audit.entries");
        assert_eq!(
            serde_json::to_value(&n).unwrap(),
            serde_json::json!("audit.entries")
        );
    }

    #[test]
    fn name_serde_rejects_invalid_nested_in_record_key() {
        use crate::record::RecordKey;
        let j = serde_json::json!({"namespace": "Bad Namespace", "key": "k"});
        assert!(serde_json::from_value::<RecordKey>(j).is_err());
        let j = serde_json::json!({"namespace": "accounts", "key": "k"});
        let rk: RecordKey = serde_json::from_value(j).unwrap();
        assert_eq!(rk.namespace.as_str(), "accounts");
    }

    #[test]
    fn name_qualified_composes() {
        let n = NamespaceName::new("accounts").unwrap();
        assert_eq!(n.qualified("maild"), "maild.accounts");
        let n = NamespaceName::new("audit.entries").unwrap();
        assert_eq!(n.qualified("auditd"), "auditd.audit.entries");
    }

    #[test]
    fn cardinality_serde() {
        let c = Cardinality::Singleton {
            canonical_key: "".into(),
        };
        let j = serde_json::to_value(&c).unwrap();
        assert_eq!(j["kind"], "singleton");
        assert_eq!(j["canonical_key"], "");

        let c = Cardinality::Collection {
            primary_key_field: "email".into(),
        };
        let j = serde_json::to_value(&c).unwrap();
        assert_eq!(j["kind"], "collection");
        assert_eq!(j["primary_key_field"], "email");
    }

    #[test]
    fn defaults_match_spec() {
        assert_eq!(DeleteMode::default(), DeleteMode::SoftDelete);
        assert_eq!(SubscribePayload::default(), SubscribePayload::Pointer);
        assert_eq!(SchemaPublic::default(), SchemaPublic::Allow);
        assert_eq!(NamespaceLifecycle::default(), NamespaceLifecycle::Simple);
        assert_eq!(
            ExternalEditPolicy::default(),
            ExternalEditPolicy::ReconcileAndContinue
        );
        assert_eq!(ConformanceLevel::default(), ConformanceLevel::L4);
        assert_eq!(SchemaVersion::default(), SchemaVersion::new(0, 1, 0));
        let r = ReplayWindow::default();
        assert_eq!(r.max_entries, 1024);
        assert_eq!(r.max_age, Duration::from_secs(3600));
    }

    #[test]
    fn spec_new_adopts_defaults() {
        let spec = NamespaceSpec::new(
            NamespaceName::new("accounts").unwrap(),
            empty_schema(),
            Cardinality::Collection {
                primary_key_field: "email".into(),
            },
            StorageBackendKind::SqliteTable {
                table: "accounts".into(),
            },
        );
        assert_eq!(spec.delete_mode, DeleteMode::SoftDelete);
        assert_eq!(spec.lifecycle, NamespaceLifecycle::Simple);
        assert_eq!(spec.tombstone_ttl, Duration::from_secs(7 * 24 * 3600));
        assert!(!spec.is_saga());
        assert_eq!(spec.fqn("maild"), "maild.accounts");
    }

    #[test]
    fn saga_flag_reflects_lifecycle() {
        let mut spec = NamespaceSpec::new(
            NamespaceName::new("accounts").unwrap(),
            empty_schema(),
            Cardinality::Collection {
                primary_key_field: "email".into(),
            },
            StorageBackendKind::SqliteTable {
                table: "accounts".into(),
            },
        );
        spec.lifecycle = NamespaceLifecycle::Saga;
        assert!(spec.is_saga());
    }

    #[test]
    fn deny_all_policy_returns_empty() {
        let p = AuthPolicy::deny_all();
        let caps = p.resolve(&PeerIdentity::default());
        assert!(caps.is_empty());
    }

    #[test]
    fn field_schema_serde_wire_shape() {
        let sch = PropertySchema::new(vec![
            FieldSchema {
                name: "email".into(),
                ty: FieldType::Email,
                default: None,
                secret: false,
                help: "Account email".into(),
                since: None,
                until: None,
                validators: vec!["email_unique".into()],
            },
            FieldSchema {
                name: "password".into(),
                ty: FieldType::String,
                default: None,
                secret: true,
                help: String::new(),
                since: None,
                until: None,
                validators: vec![],
            },
        ]);
        let j = serde_json::to_value(&sch).unwrap();
        let fields = j["fields"].as_array().unwrap();
        assert_eq!(fields[0]["name"], "email");
        // SPEC 12 §8.3: primitives serialise as bare strings.
        assert_eq!(fields[0]["type"], "email");
        assert!(fields[0].get("secret").is_none(), "false elided");
        assert_eq!(fields[0]["validators"][0], "email_unique");
        assert_eq!(fields[1]["name"], "password");
        assert_eq!(fields[1]["type"], "string");
        assert_eq!(fields[1]["secret"], true);
        assert!(fields[1].get("validators").is_none(), "empty elided");
        assert!(fields[1].get("help").is_none(), "empty elided");
        let back: PropertySchema = serde_json::from_value(j).unwrap();
        assert_eq!(back.fields.len(), 2);
        assert_eq!(back.fields[0].validators, vec!["email_unique"]);
    }

    #[test]
    fn field_type_primitive_is_bare_string() {
        // §8.3 wire shape: `"type": "string"`.
        let j = serde_json::to_value(FieldType::String).unwrap();
        assert_eq!(j, serde_json::json!("string"));
        let back: FieldType = serde_json::from_value(j).unwrap();
        assert!(matches!(back, FieldType::String));
        // All primitive tokens round-trip.
        for tok in [
            "bool", "i64", "u64", "f64", "string", "bytes", "path", "email", "url",
        ] {
            let t: FieldType =
                serde_json::from_value(serde_json::Value::String(tok.into())).unwrap();
            let back_j = serde_json::to_value(&t).unwrap();
            assert_eq!(back_j, serde_json::json!(tok), "round-trip {tok}");
        }
    }

    #[test]
    fn field_type_compound_round_trip() {
        let t = FieldType::List {
            item: Box::new(FieldType::Enum {
                variants: vec!["smtp".into(), "imap".into()],
            }),
        };
        let j = serde_json::to_value(&t).unwrap();
        assert_eq!(j["kind"], "list");
        assert_eq!(j["item"]["kind"], "enum");
        assert_eq!(j["item"]["variants"][1], "imap");
        let back: FieldType = serde_json::from_value(j).unwrap();
        // Round-trips. Confirm via re-serialise rather than PartialEq
        // (FieldType is not Eq because of nested PropValue defaults).
        let j2 = serde_json::to_value(&back).unwrap();
        assert_eq!(j2["kind"], "list");
        assert_eq!(j2["item"]["kind"], "enum");
    }

    #[test]
    fn field_type_unknown_primitive_rejected() {
        let err = serde_json::from_value::<FieldType>(serde_json::json!("blob")).unwrap_err();
        assert!(
            err.to_string().contains("unknown FieldType primitive"),
            "got: {err}"
        );
    }

    #[test]
    fn schema_version_dotted_wire_shape() {
        // §8.3: `"schema_version": "0.1.0"`.
        let v = SchemaVersion::new(0, 1, 0);
        assert_eq!(v.to_string(), "0.1.0");
        let j = serde_json::to_value(v).unwrap();
        assert_eq!(j, serde_json::json!("0.1.0"));
        let back: SchemaVersion = serde_json::from_value(j).unwrap();
        assert_eq!(back, v);
        // Component-wise ordering, not lex on the wire string.
        assert!(SchemaVersion::new(0, 10, 0) > SchemaVersion::new(0, 9, 0));
        assert!(SchemaVersion::new(1, 0, 0) > SchemaVersion::new(0, 99, 99));
    }

    #[test]
    fn schema_version_parse_rejects_bad_inputs() {
        let cases = [
            serde_json::json!("0.1"),
            serde_json::json!("0.1.0.0"),
            serde_json::json!("0.x.0"),
            serde_json::json!(""),
        ];
        for c in cases {
            assert!(
                serde_json::from_value::<SchemaVersion>(c.clone()).is_err(),
                "should reject {c}"
            );
        }
    }

    #[test]
    fn validator_secret_flag_present() {
        let v = Validator {
            name: "email_unique",
            validator_secret: false,
            check: Arc::new(|_| Ok(())),
        };
        assert!(!v.validator_secret);
        let v2 = Validator {
            name: "password_pattern",
            validator_secret: true,
            check: Arc::new(|_| Ok(())),
        };
        assert!(v2.validator_secret);
    }
}
