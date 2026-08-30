//! `interact.*` wire contract — the headless serde DTOs for the ctkd
//! ephemeral-surfaces daemon.
//!
//! This crate is the **single source of truth for the interaction vocabulary**
//! (the ctkd ADR §3's "one vocabulary, two representations" split): non-Bevy
//! callers (Mix scripts, daemons) depend on these DTOs without pulling the
//! toolkit. CTK will take this dependency when `dialog.v1` lands in Phase 2/3
//! and map DTOs to the live in-process requests that drive the same widgets.
//! Pure Rust, no Bevy/cosmix deps — a sibling of `cosmix-mixer-schema` and
//! `cosmix-song`.
//!
//! The crate currently defines `notify.v1` plus the headless `dialog.v1`
//! contract. Transport verbs and the Bevy presenter are deliberately outside
//! this crate.
//!
//! Two invariants encoded here, both from the ratified specs:
//!
//! - **No `origin` on the request.** Provenance is broker-stamped from the Bus
//!   `from` and cannot be set by the caller (notify.v1 §4, the buildable slice of
//!   the B-2 mitigation). It appears only on [`NotifyRecord`], the broker-owned
//!   stored form.
//! - **No confidential notification fields.** notify.v1 content has no
//!   `sensitive`/password vocabulary. The separate [`OwnerToken`] envelope
//!   field is an opaque mutation capability and must not be logged, rendered,
//!   or projected through props.

use serde::{Deserialize, Serialize};

mod dialog;

pub use dialog::*;

/// Maximum actions attached to one notification (freedesktop toasts realistically
/// surface only a few; notify.v1 §2 caps at ~3).
pub const MAX_ACTIONS: usize = 3;
/// Maximum UTF-8 bytes in a notification summary.
pub const MAX_SUMMARY_BYTES: usize = 512;
/// Maximum UTF-8 bytes in a notification body.
pub const MAX_BODY_BYTES: usize = 8 * 1_024;
/// Maximum UTF-8 bytes in an action label.
pub const MAX_ACTION_LABEL_BYTES: usize = 128;
/// Maximum UTF-8 bytes in caller-defined keys (action, icon, and dedupe).
pub const MAX_KEY_BYTES: usize = 128;
/// Maximum UTF-8 bytes in the freedesktop category hint.
pub const MAX_CATEGORY_BYTES: usize = 128;
/// Maximum logical content bytes retained by one [`NotifyRequest`].
pub const MAX_REQUEST_BYTES: usize = 16 * 1_024;
/// Maximum logical request bytes retained across interactd's delivery queue.
pub const MAX_DELIVERY_QUEUE_BYTES: usize = 128 * 1_024;

/// Stable wire error id for an overlong field.
pub const VALIDATION_FIELD_TOO_LONG: &str = "validation_field_too_long";
/// Stable wire error id for a request whose aggregate content is too large.
pub const VALIDATION_REQUEST_TOO_LARGE: &str = "validation_request_too_large";

// --------------------------------------------------------------------------
// Request — what a caller sends over `interact.notify`.
// --------------------------------------------------------------------------

/// A passive notification request (`notify.v1`).
///
/// Deliberately has **no** `origin` field: the broker derives origin from the
/// caller's registered Bus identity and the caller cannot override it. Construct,
/// then [`NotifyRequest::validate`] before dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifyRequest {
    /// Short single-line headline. Required, non-empty.
    pub summary: String,
    /// Optional longer text. Plain — no markup, no layout (the ui.* lane is
    /// retired; the host owns presentation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Attention level. Remote callers are clamped to `<= Normal` by the broker
    /// (notify.v1 §10.3); this type does not enforce that — the broker does.
    #[serde(default)]
    pub urgency: Urgency,
    /// freedesktop category hint (e.g. `"device"`, `"transfer.complete"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Icon by catalogue key only — never image bytes or filesystem paths. v1
    /// resolves [`IconRef::Lucide`] only; [`IconRef::Emoji`] returns the
    /// notify.v1 `unsupported` result until the emoji asset class ships.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<IconRef>,
    /// Coalesce/replace a prior notification carrying the same key rather than
    /// stacking a new one (maps to the freedesktop `replaces_id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedupe_key: Option<String>,
    /// Display-timeout hint in ms. `None` = desktop-daemon default; `Some(0)`
    /// and values above one hour are clamped to the interactd policy maximum.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u32>,
    /// Optional actions. Each dispatch target must still be a registered service
    /// when clicked (never a pid), so stale targets are dropped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<NotifyAction>,
}

/// Wire envelope accepted by `interact.notify`.
///
/// `owner_token` is absent for a fresh notification. A caller replacing an
/// existing `dedupe_key` must present the token returned with that handle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifyCreateRequest {
    #[serde(flatten)]
    pub request: NotifyRequest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_token: Option<OwnerToken>,
}

impl NotifyCreateRequest {
    pub fn new(request: NotifyRequest) -> Self {
        Self {
            request,
            owner_token: None,
        }
    }
}

/// Attention level for a notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Urgency {
    Low,
    #[default]
    Normal,
    Critical,
}

/// An icon referenced by curated-catalogue key. The DTO carries both catalogues
/// for forward-compat; broker-side resolution is Lucide-only in v1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IconRef {
    /// A key into the shared Lucide `Icon` catalogue (`ctk::icons`).
    Lucide(String),
    /// A key into the emoji catalogue (emoji-as-a-shared-CTK-resource ADR).
    /// Returns `unsupported` until that asset class ships.
    Emoji(String),
}

/// One actionable button on a notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifyAction {
    /// Stable id echoed back to the dispatch target on invocation.
    pub key: String,
    /// Human-visible label.
    pub label: String,
    /// What the broker fires when the user clicks. `None` = dismiss + record only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_invoke: Option<Dispatch>,
}

/// A callback the broker fires on action invocation: `send <service> <verb>
/// handle=<h> key=<k> requested_by=<service>`. The Bus sender is `interact`;
/// `requested_by` is the notify-time broker-stamped requesting service. Receivers
/// must treat the handle, key, and attribution as untrusted input and apply their
/// own authorisation policy. The target service is checked both before display
/// and again at click time, never interpreted as a process id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dispatch {
    pub service: String,
    pub verb: String,
}

// --------------------------------------------------------------------------
// Handle + broker-owned record.
// --------------------------------------------------------------------------

/// Opaque interaction handle. Unguessable, stable, and distinct from the Bus
/// `id` (which is transport correlation, rewritten broker-side). Existing
/// notification handles retain their `n` prefix; dialog handles use `d`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InteractionHandle(pub String);

impl InteractionHandle {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Backwards-compatible source alias for notification callers.
pub use InteractionHandle as NotifyHandle;

/// Unguessable capability authorising mutation of one notification. It is
/// returned only to the creator and never projected through props. interactd
/// also requires every update, dismissal, and dedupe replacement to come from
/// the same broker-stamped service that minted the handle, so a token observed
/// through `noded.tap` is useless to another caller. Residual: a service that
/// disconnects can currently be replaced by a same-name registration; closing
/// that gap requires the control-plane authority work planned for B-2/B-3.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerToken(pub String);

impl OwnerToken {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Successful `interact.notify` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifyResponse {
    pub handle: NotifyHandle,
    pub owner_token: OwnerToken,
}

/// Token-bearing `interact.update` request. The notification fields remain
/// flattened on the wire for compatibility with the original notify.v1 shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifyUpdateRequest {
    pub handle: NotifyHandle,
    pub owner_token: OwnerToken,
    #[serde(flatten)]
    pub request: NotifyRequest,
}

/// Token-bearing `interact.dismiss` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifyDismissRequest {
    pub handle: NotifyHandle,
    pub owner_token: OwnerToken,
}

/// Immediate acknowledgement for an accepted update or dismissal. Delivery is
/// asynchronous: `queued` means the sink has not resolved yet, while a terminal
/// state records the accepted logical transition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifyMutationResponse {
    pub handle: NotifyHandle,
    pub state: NotifyState,
}

/// Terminal-or-transient lifecycle state of a live notification. Exactly one
/// terminal state is reached per notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotifyState {
    /// Accepted, not yet handed to the daemon.
    Queued,
    /// Presented by the notification daemon.
    Shown,
    // --- terminal ---
    /// Dismissed (by the user or `interact.dismiss`).
    Dismissed,
    /// Timed out per its display timeout.
    Expired,
    /// An action was invoked; carries its own dispatch (recorded separately).
    ActionInvoked,
    /// Delivery failed (no daemon, transport error, …).
    Failed,
}

impl NotifyState {
    /// Whether this is a terminal state (no further transitions).
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            NotifyState::Dismissed
                | NotifyState::Expired
                | NotifyState::ActionInvoked
                | NotifyState::Failed
        )
    }
}

/// The broker-owned stored form of a live notification — the shape held in the
/// SPEC-12 memory-backed `interactions` props collection (notify.v1 §7). Unlike
/// [`NotifyRequest`] it carries the **broker-stamped** `origin` and `handle`;
/// `created_at_ms` is supplied by the broker (this crate calls no clock).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifyRecord {
    pub handle: NotifyHandle,
    /// Non-spoofable provenance: the caller's registered Bus identity. Never
    /// caller-supplied. notify.v1 rejects anonymous ingress, and reserved labels
    /// (`system`/`cosmix`/`root`) are unassignable in v1 (empty allowlist —
    /// notify.v1 §10.2).
    pub origin: String,
    pub state: NotifyState,
    /// Wall-clock creation time in ms since the Unix epoch, stamped by the broker.
    pub created_at_ms: u64,
    pub summary: String,
    /// Urgency after broker policy, present for every record rather than only
    /// when a remote Critical request required a clamp.
    #[serde(default)]
    pub effective_urgency: Urgency,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub urgency_override: Option<Urgency>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedupe_key: Option<String>,
}

// --------------------------------------------------------------------------
// Validation.
// --------------------------------------------------------------------------

/// Why a [`NotifyRequest`] was rejected. Structural only — capability/origin/
/// rate checks are the broker's, not the schema's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    /// `summary` was empty or whitespace-only.
    EmptySummary,
    /// More than [`MAX_ACTIONS`] actions were attached.
    TooManyActions { got: usize, max: usize },
    /// An action had an empty `key`.
    EmptyActionKey { index: usize },
    /// Two actions shared a `key` (they must be distinct — the key is echoed back).
    DuplicateActionKey { key: String },
    /// An action's `on_invoke` had an empty `service` or `verb`.
    EmptyDispatchTarget { index: usize },
    /// An action's dispatch service or verb did not match its wire grammar.
    InvalidDispatchIdentifier {
        index: usize,
        field: &'static str,
        value: String,
    },
    /// A bounded string field exceeded its UTF-8 byte cap.
    FieldTooLong {
        field: &'static str,
        index: Option<usize>,
        got: usize,
        max: usize,
    },
    /// Aggregate request content exceeded [`MAX_REQUEST_BYTES`].
    RequestTooLarge { got: usize, max: usize },
}

impl ValidationError {
    /// Stable application error id suitable for an Bus response.
    pub fn code(&self) -> &'static str {
        match self {
            Self::FieldTooLong { .. } => VALIDATION_FIELD_TOO_LONG,
            Self::RequestTooLarge { .. } => VALIDATION_REQUEST_TOO_LARGE,
            _ => "validation_error",
        }
    }
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ValidationError::EmptySummary => write!(f, "notify summary must not be empty"),
            ValidationError::TooManyActions { got, max } => {
                write!(f, "too many actions: {got} > {max}")
            }
            ValidationError::EmptyActionKey { index } => {
                write!(f, "action[{index}] has an empty key")
            }
            ValidationError::DuplicateActionKey { key } => {
                write!(f, "duplicate action key {key:?}")
            }
            ValidationError::EmptyDispatchTarget { index } => {
                write!(f, "action[{index}] on_invoke has an empty service/verb")
            }
            ValidationError::InvalidDispatchIdentifier {
                index,
                field,
                value,
            } => {
                let grammar = if *field == "service" {
                    "the Bus service-name grammar"
                } else {
                    "the SPEC-02 command grammar"
                };
                write!(
                    f,
                    "action[{index}] on_invoke {field} does not match {grammar}: {value:?}"
                )
            }
            ValidationError::FieldTooLong {
                field,
                index,
                got,
                max,
            } => {
                if let Some(index) = index {
                    write!(
                        f,
                        "action[{index}] {field} is too long: {got} bytes > {max}"
                    )
                } else {
                    write!(f, "{field} is too long: {got} bytes > {max}")
                }
            }
            ValidationError::RequestTooLarge { got, max } => {
                write!(f, "notify request is too large: {got} bytes > {max}")
            }
        }
    }
}

impl std::error::Error for ValidationError {}

impl NotifyRequest {
    /// A minimal request with just a headline.
    pub fn new(summary: impl Into<String>) -> Self {
        NotifyRequest {
            summary: summary.into(),
            body: None,
            urgency: Urgency::default(),
            category: None,
            icon: None,
            dedupe_key: None,
            timeout_ms: None,
            actions: Vec::new(),
        }
    }

    /// Structural validation — call before dispatch. Does **not** check
    /// capability, origin, rate limits, or that a `Dispatch.service` is actually
    /// registered; those are broker responsibilities.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.summary.trim().is_empty() {
            return Err(ValidationError::EmptySummary);
        }
        validate_len("summary", None, &self.summary, MAX_SUMMARY_BYTES)?;
        if let Some(body) = &self.body {
            validate_len("body", None, body, MAX_BODY_BYTES)?;
        }
        if let Some(category) = &self.category {
            validate_len("category", None, category, MAX_CATEGORY_BYTES)?;
        }
        if let Some(icon) = &self.icon {
            let key = match icon {
                IconRef::Lucide(key) | IconRef::Emoji(key) => key,
            };
            validate_len("icon key", None, key, MAX_KEY_BYTES)?;
        }
        if let Some(key) = &self.dedupe_key {
            validate_len("dedupe_key", None, key, MAX_KEY_BYTES)?;
        }
        if self.actions.len() > MAX_ACTIONS {
            return Err(ValidationError::TooManyActions {
                got: self.actions.len(),
                max: MAX_ACTIONS,
            });
        }
        let mut seen: Vec<&str> = Vec::with_capacity(self.actions.len());
        for (index, action) in self.actions.iter().enumerate() {
            if action.key.trim().is_empty() {
                return Err(ValidationError::EmptyActionKey { index });
            }
            validate_len("key", Some(index), &action.key, MAX_KEY_BYTES)?;
            validate_len("label", Some(index), &action.label, MAX_ACTION_LABEL_BYTES)?;
            if seen.contains(&action.key.as_str()) {
                return Err(ValidationError::DuplicateActionKey {
                    key: action.key.clone(),
                });
            }
            seen.push(&action.key);
            if let Some(d) = &action.on_invoke {
                if d.service.trim().is_empty() || d.verb.trim().is_empty() {
                    return Err(ValidationError::EmptyDispatchTarget { index });
                }
                if !is_bus_service_name(&d.service) {
                    return Err(ValidationError::InvalidDispatchIdentifier {
                        index,
                        field: "service",
                        value: d.service.clone(),
                    });
                }
                if !is_bus_command_name(&d.verb) {
                    return Err(ValidationError::InvalidDispatchIdentifier {
                        index,
                        field: "verb",
                        value: d.verb.clone(),
                    });
                }
            }
        }
        let got = self.retained_bytes();
        if got > MAX_REQUEST_BYTES {
            return Err(ValidationError::RequestTooLarge {
                got,
                max: MAX_REQUEST_BYTES,
            });
        }
        Ok(())
    }

    /// Logical UTF-8 content bytes retained by this request. This excludes
    /// fixed struct/JSON punctuation overhead and is used for the aggregate
    /// request and delivery-queue budgets.
    pub fn retained_bytes(&self) -> usize {
        let mut bytes = self.summary.len();
        bytes = bytes.saturating_add(self.body.as_ref().map_or(0, String::len));
        bytes = bytes.saturating_add(self.category.as_ref().map_or(0, String::len));
        bytes = bytes.saturating_add(self.dedupe_key.as_ref().map_or(0, String::len));
        bytes = bytes.saturating_add(self.icon.as_ref().map_or(0, |icon| match icon {
            IconRef::Lucide(key) | IconRef::Emoji(key) => key.len(),
        }));
        for action in &self.actions {
            bytes = bytes
                .saturating_add(action.key.len())
                .saturating_add(action.label.len());
            if let Some(dispatch) = &action.on_invoke {
                bytes = bytes
                    .saturating_add(dispatch.service.len())
                    .saturating_add(dispatch.verb.len());
            }
        }
        bytes
    }
}

fn validate_len(
    field: &'static str,
    index: Option<usize>,
    value: &str,
    max: usize,
) -> Result<(), ValidationError> {
    if value.len() > max {
        Err(ValidationError::FieldTooLong {
            field,
            index,
            got: value.len(),
            max,
        })
    } else {
        Ok(())
    }
}

/// SPEC-10 R6's Bus service-name grammar:
/// `^[a-z][a-z0-9-]{1,30}$` (2–31 ASCII bytes total).
pub fn is_bus_service_name(value: &str) -> bool {
    let bytes = value.as_bytes();
    (2..=31).contains(&bytes.len())
        && bytes[0].is_ascii_lowercase()
        && bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

/// SPEC-02 command grammar: `<service>.<verb>[-<noun>]`, lowercase ASCII,
/// with hyphens separating noun segments and no underscores or extra dots.
fn is_bus_command_name(value: &str) -> bool {
    let Some((service, command)) = value.split_once('.') else {
        return false;
    };
    if command.contains('.') || !is_bus_service_name(service) {
        return false;
    }
    command.split('-').all(|segment| {
        let mut bytes = segment.bytes();
        bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
            && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_request_validates() {
        assert!(NotifyRequest::new("Build finished").validate().is_ok());
    }

    #[test]
    fn empty_summary_rejected() {
        assert_eq!(
            NotifyRequest::new("   ").validate(),
            Err(ValidationError::EmptySummary)
        );
    }

    #[test]
    fn too_many_actions_rejected() {
        let mut r = NotifyRequest::new("x");
        r.actions = (0..MAX_ACTIONS + 1)
            .map(|i| NotifyAction {
                key: format!("k{i}"),
                label: "l".into(),
                on_invoke: None,
            })
            .collect();
        assert_eq!(
            r.validate(),
            Err(ValidationError::TooManyActions {
                got: MAX_ACTIONS + 1,
                max: MAX_ACTIONS
            })
        );
    }

    #[test]
    fn per_field_byte_caps_are_enforced() {
        let mut request = NotifyRequest::new("x".repeat(MAX_SUMMARY_BYTES + 1));
        assert_eq!(
            request.validate(),
            Err(ValidationError::FieldTooLong {
                field: "summary",
                index: None,
                got: MAX_SUMMARY_BYTES + 1,
                max: MAX_SUMMARY_BYTES,
            })
        );

        request = NotifyRequest::new("x");
        request.body = Some("é".repeat(MAX_BODY_BYTES / 2 + 1));
        assert_eq!(
            request.validate(),
            Err(ValidationError::FieldTooLong {
                field: "body",
                index: None,
                got: MAX_BODY_BYTES + 2,
                max: MAX_BODY_BYTES,
            })
        );

        request = NotifyRequest::new("x");
        request.actions.push(NotifyAction {
            key: "go".into(),
            label: "x".repeat(MAX_ACTION_LABEL_BYTES + 1),
            on_invoke: None,
        });
        assert_eq!(
            request.validate(),
            Err(ValidationError::FieldTooLong {
                field: "label",
                index: Some(0),
                got: MAX_ACTION_LABEL_BYTES + 1,
                max: MAX_ACTION_LABEL_BYTES,
            })
        );
    }

    #[test]
    fn aggregate_request_byte_cap_is_enforced() {
        let mut request = NotifyRequest::new("x".repeat(MAX_SUMMARY_BYTES));
        request.body = Some("b".repeat(MAX_BODY_BYTES));
        request.category = Some("c".repeat(MAX_CATEGORY_BYTES));
        request.dedupe_key = Some("d".repeat(MAX_KEY_BYTES));
        request.icon = Some(IconRef::Lucide("i".repeat(MAX_KEY_BYTES)));
        request.actions = (0..MAX_ACTIONS)
            .map(|index| NotifyAction {
                key: format!("{index}{}", "k".repeat(MAX_KEY_BYTES - 1)),
                label: "l".repeat(MAX_ACTION_LABEL_BYTES),
                on_invoke: Some(Dispatch {
                    service: "target".into(),
                    verb: format!("app.{}", "v".repeat(2_500)),
                }),
            })
            .collect();
        let got = request.retained_bytes();
        assert!(got > MAX_REQUEST_BYTES);
        assert_eq!(
            request.validate(),
            Err(ValidationError::RequestTooLarge {
                got,
                max: MAX_REQUEST_BYTES,
            })
        );
    }

    #[test]
    fn duplicate_action_key_rejected() {
        let mut r = NotifyRequest::new("x");
        r.actions = vec![
            NotifyAction {
                key: "open".into(),
                label: "Open".into(),
                on_invoke: None,
            },
            NotifyAction {
                key: "open".into(),
                label: "Open again".into(),
                on_invoke: None,
            },
        ];
        assert_eq!(
            r.validate(),
            Err(ValidationError::DuplicateActionKey { key: "open".into() })
        );
    }

    #[test]
    fn empty_dispatch_target_rejected() {
        let mut r = NotifyRequest::new("x");
        r.actions = vec![NotifyAction {
            key: "go".into(),
            label: "Go".into(),
            on_invoke: Some(Dispatch {
                service: "".into(),
                verb: "open".into(),
            }),
        }];
        assert_eq!(
            r.validate(),
            Err(ValidationError::EmptyDispatchTarget { index: 0 })
        );
    }

    #[test]
    fn dispatch_service_and_verb_reject_header_injection_payloads() {
        for payload in ["app.quit\nforged", "---", "app quit"] {
            for field in ["service", "verb"] {
                let mut r = NotifyRequest::new("x");
                let mut dispatch = Dispatch {
                    service: "filemgr".into(),
                    verb: "app.quit".into(),
                };
                match field {
                    "service" => dispatch.service = payload.into(),
                    "verb" => dispatch.verb = payload.into(),
                    _ => unreachable!(),
                }
                r.actions = vec![NotifyAction {
                    key: "go".into(),
                    label: "Go".into(),
                    on_invoke: Some(dispatch),
                }];
                assert_eq!(
                    r.validate(),
                    Err(ValidationError::InvalidDispatchIdentifier {
                        index: 0,
                        field,
                        value: payload.into(),
                    }),
                    "field={field} payload={payload:?}"
                );
            }
        }
    }

    #[test]
    fn dispatch_service_and_command_use_their_spec_grammars() {
        for (service, verb) in [
            ("filemgr", "app.reveal"),
            ("musicd-2", "transport.stop"),
            ("test-service", "edit.get-content"),
        ] {
            let mut r = NotifyRequest::new("x");
            r.actions = vec![NotifyAction {
                key: "go".into(),
                label: "Go".into(),
                on_invoke: Some(Dispatch {
                    service: service.into(),
                    verb: verb.into(),
                }),
            }];
            assert!(r.validate().is_ok(), "{service}.{verb}");
        }
    }

    #[test]
    fn dispatch_command_rejects_non_spec02_names() {
        for verb in [
            "open",
            "app.Reveal",
            "app.get_content",
            "app.get--content",
            "app.get-content.more",
            "_internal.run",
            "app.-content",
        ] {
            let mut r = NotifyRequest::new("x");
            r.actions = vec![NotifyAction {
                key: "go".into(),
                label: "Go".into(),
                on_invoke: Some(Dispatch {
                    service: "filemgr".into(),
                    verb: verb.into(),
                }),
            }];
            assert_eq!(
                r.validate(),
                Err(ValidationError::InvalidDispatchIdentifier {
                    index: 0,
                    field: "verb",
                    value: verb.into(),
                }),
                "verb={verb:?}"
            );
        }
    }

    #[test]
    fn ownership_wire_envelopes_round_trip() {
        let response = NotifyResponse {
            handle: NotifyHandle("n123".into()),
            owner_token: OwnerToken("o123".into()),
        };
        let response_json = serde_json::to_string(&response).unwrap();
        assert_eq!(
            serde_json::from_str::<NotifyResponse>(&response_json).unwrap(),
            response
        );

        let update = NotifyUpdateRequest {
            handle: NotifyHandle("n123".into()),
            owner_token: OwnerToken("o123".into()),
            request: NotifyRequest::new("updated"),
        };
        let update_json = serde_json::to_string(&update).unwrap();
        assert_eq!(update_json.matches("summary").count(), 1);
        assert_eq!(
            serde_json::from_str::<NotifyUpdateRequest>(&update_json).unwrap(),
            update
        );

        let dismiss = NotifyDismissRequest {
            handle: NotifyHandle("n123".into()),
            owner_token: OwnerToken("o123".into()),
        };
        let dismiss_json = serde_json::to_string(&dismiss).unwrap();
        assert_eq!(
            serde_json::from_str::<NotifyDismissRequest>(&dismiss_json).unwrap(),
            dismiss
        );

        let mutation = NotifyMutationResponse {
            handle: NotifyHandle("n123".into()),
            state: NotifyState::Queued,
        };
        let mutation_json = serde_json::to_string(&mutation).unwrap();
        assert_eq!(
            serde_json::from_str::<NotifyMutationResponse>(&mutation_json).unwrap(),
            mutation
        );
    }

    #[test]
    fn dispatch_service_rejects_non_bus_service_names() {
        for service in [
            "x",
            "foo.bar",
            "foo_bar",
            "Foo",
            "foo bar",
            "foo\nbar",
            "service-name-that-is-over-thirty-one-chars",
        ] {
            let mut r = NotifyRequest::new("x");
            r.actions = vec![NotifyAction {
                key: "go".into(),
                label: "Go".into(),
                on_invoke: Some(Dispatch {
                    service: service.into(),
                    verb: "app.reveal".into(),
                }),
            }];
            assert_eq!(
                r.validate(),
                Err(ValidationError::InvalidDispatchIdentifier {
                    index: 0,
                    field: "service",
                    value: service.into(),
                }),
                "service={service:?}"
            );
        }
    }

    #[test]
    fn request_has_no_origin_field_on_the_wire() {
        // The wire form must not carry provenance — origin is broker-stamped.
        let json = serde_json::to_string(&NotifyRequest::new("hi")).unwrap();
        assert!(
            !json.contains("origin"),
            "request JSON leaked an origin: {json}"
        );
    }

    #[test]
    fn urgency_serialises_lowercase() {
        assert_eq!(
            serde_json::to_string(&Urgency::Critical).unwrap(),
            "\"critical\""
        );
    }

    #[test]
    fn request_round_trips() {
        let mut r = NotifyRequest::new("Render finished");
        r.body = Some("studio 0.2.9 built".into());
        r.urgency = Urgency::Low;
        r.icon = Some(IconRef::Lucide("check".into()));
        r.dedupe_key = Some("render".into());
        r.actions = vec![NotifyAction {
            key: "reveal".into(),
            label: "Reveal".into(),
            on_invoke: Some(Dispatch {
                service: "filemgr".into(),
                verb: "app.reveal".into(),
            }),
        }];
        r.validate().unwrap();
        let json = serde_json::to_string(&r).unwrap();
        let back: NotifyRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(r, back);
    }

    #[test]
    fn state_terminality() {
        assert!(!NotifyState::Queued.is_terminal());
        assert!(!NotifyState::Shown.is_terminal());
        assert!(NotifyState::Dismissed.is_terminal());
        assert!(NotifyState::Failed.is_terminal());
    }

    #[test]
    fn record_round_trips() {
        let rec = NotifyRecord {
            handle: NotifyHandle("h-abc123".into()),
            origin: "musicd".into(),
            state: NotifyState::Shown,
            created_at_ms: 1_700_000_000_000,
            summary: "Loaded".into(),
            effective_urgency: Urgency::Normal,
            urgency_override: None,
            dedupe_key: None,
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: NotifyRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(rec, back);
    }
}
