//! Strict `dialog.v1` DTOs and structural validation.
//!
//! # Raw-ingress invariant
//!
//! Serde allocates strings and vectors while deserialising, before
//! [`DialogRequestV1::validate`] or [`DialogValueV1::validate_for`] can run.
//! Therefore every transport adapter **must** reject a raw request body larger
//! than [`DIALOG_RAW_INGRESS_CAP`] before calling `serde_json::from_slice`.
//! Per-field, collection-count, and retained-content validation is defence in
//! depth after that mandatory allocation bound; it is not a replacement for
//! the raw check.
//! Additive fields are a `dialog.v2` negotiation concern; v1 is strict by
//! design.

use std::collections::HashSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{InteractionHandle, OwnerToken};

/// Mandatory pre-deserialisation cap for every `dialog.v1` request body.
pub const DIALOG_RAW_INGRESS_CAP: usize = 16 * 1024;
pub const VERB_DIALOG_OPEN: &str = "interact.dialog-open";
pub const VERB_DIALOG_PROGRESS_UPDATE: &str = "interact.dialog-progress-update";
pub const VERB_DIALOG_PROGRESS_COMPLETE: &str = "interact.dialog-progress-complete";
pub const VERB_DIALOG_CANCEL: &str = "interact.dialog-cancel";
pub const VERB_DIALOG_RESULT: &str = "interact.dialog-result";
pub const VERB_PRESENTER_REGISTER: &str = "interact.presenter-register";
pub const VERB_PRESENTER_RELEASE: &str = "interact.presenter-release";
pub const VERB_PRESENTER_NEXT: &str = "interact.presenter-next";
pub const VERB_PRESENTER_MARK_PRESENTED: &str = "interact.presenter-mark-presented";
pub const VERB_PRESENTER_RESOLVE: &str = "interact.presenter-resolve";
pub const VERB_PRESENTER_FAIL: &str = "interact.presenter-fail";
pub const VERB_PRESENTER_PROGRESS_CANCEL: &str = "interact.presenter-progress-cancel";
pub const TOPIC_INTERACT_PROPS_CHANGED: &str = "interact.props.changed";
pub const MIN_DIALOG_DEADLINE_MS: u64 = 300_000;
pub const MAX_DIALOG_REQUEST_BYTES: usize = DIALOG_RAW_INGRESS_CAP;
pub const MAX_DIALOG_RESULT_BYTES: usize = DIALOG_RAW_INGRESS_CAP;
pub const MAX_DIALOG_TITLE_BYTES: usize = 512;
pub const MAX_DIALOG_MESSAGE_BYTES: usize = 8 * 1024;
pub const MAX_DIALOG_DETAILS_BYTES: usize = 8 * 1024;
pub const MAX_DIALOG_TEXT_VIEW_BYTES: usize = 12 * 1024;
pub const MAX_DIALOG_LABEL_BYTES: usize = 128;
pub const MAX_DIALOG_KEY_BYTES: usize = 128;
pub const MAX_DIALOG_PATH_BYTES: usize = 4 * 1024;
pub const MAX_DIALOG_PROMPT_INITIAL_BYTES: usize = 4 * 1024;
pub const MAX_DIALOG_PROMPT_CHARS: u32 = 4_096;
pub const MAX_DIALOG_ITEMS: usize = 256;
pub const MAX_DIALOG_PATHS: usize = 256;
pub const MAX_DIALOG_ACTIONS: usize = 3;
pub const MAX_DIALOG_FILTERS: usize = 16;
pub const MAX_DIALOG_EXTENSIONS_PER_FILTER: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogValidationError {
    Empty(&'static str),
    TooLong { field: &'static str, max: usize },
    TooMany { field: &'static str, max: usize },
    InvalidKey { field: &'static str, key: String },
    DuplicateKey { field: &'static str, key: String },
    MultipleDefaults,
    MultipleCancelActions,
    DestructiveDefault,
    UnknownDefaultKey(String),
    DisabledDefaultKey(String),
    InvalidProgress,
    InvalidSlider,
    InvalidSaveName,
    InvalidExtension(String),
    InvalidDeadline,
    ResultKindMismatch,
    UnknownResultKey(String),
    DisabledResultKey(String),
    DuplicateResultKey(String),
    RequestTooLarge { got: usize, max: usize },
    ResultTooLarge { got: usize, max: usize },
}

impl fmt::Display for DialogValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty(field) => write!(f, "{field} must not be empty"),
            Self::TooLong { field, max } => write!(f, "{field} exceeds {max} bytes"),
            Self::TooMany { field, max } => write!(f, "{field} exceeds {max} items"),
            Self::InvalidKey { field, key } => {
                write!(f, "{field} contains invalid key {key:?}")
            }
            Self::DuplicateKey { field, key } => {
                write!(f, "{field} contains duplicate key {key:?}")
            }
            Self::MultipleDefaults => write!(f, "at most one action may be the default"),
            Self::MultipleCancelActions => write!(f, "at most one action may cancel"),
            Self::DestructiveDefault => write!(f, "a destructive action cannot be the default"),
            Self::UnknownDefaultKey(key) => write!(f, "default key {key:?} does not exist"),
            Self::DisabledDefaultKey(key) => write!(f, "default key {key:?} is disabled"),
            Self::InvalidProgress => {
                write!(
                    f,
                    "determinate progress requires total > 0 and current <= total"
                )
            }
            Self::InvalidSlider => write!(
                f,
                "slider requires min < max, step > 0, and an aligned in-range value"
            ),
            Self::InvalidSaveName => write!(f, "suggested save name must be a basename"),
            Self::InvalidExtension(value) => {
                write!(f, "extension {value:?} is not normalised")
            }
            Self::InvalidDeadline => write!(
                f,
                "deadline_ms must be absent or at least {MIN_DIALOG_DEADLINE_MS}"
            ),
            Self::ResultKindMismatch => {
                write!(f, "result variant does not match the dialog kind")
            }
            Self::UnknownResultKey(key) => write!(f, "result key {key:?} does not exist"),
            Self::DisabledResultKey(key) => write!(f, "result key {key:?} is disabled"),
            Self::DuplicateResultKey(key) => write!(f, "result key {key:?} is duplicated"),
            Self::RequestTooLarge { got, max } => {
                write!(f, "dialog request is too large: {got} bytes > {max}")
            }
            Self::ResultTooLarge { got, max } => {
                write!(f, "dialog result is too large: {got} bytes > {max}")
            }
        }
    }
}

impl std::error::Error for DialogValidationError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub enum DialogKindV1 {
    Message,
    Confirm,
    Prompt,
    Choice,
    MultiChoice,
    Progress,
    FileOpen,
    FileSave,
    DirSelect,
    Slider,
    TextView,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub enum DialogSeverityV1 {
    #[default]
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialogCommonV1 {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default)]
    pub severity: DialogSeverityV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub enum DialogActionRoleV1 {
    Accept,
    Cancel,
    Destructive,
    Auxiliary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialogActionV1 {
    pub key: String,
    pub label: String,
    pub role: DialogActionRoleV1,
    #[serde(default)]
    pub is_default: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialogChoiceItemV1 {
    pub key: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialogTextInputV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial: Option<String>,
    #[serde(default = "default_prompt_chars")]
    pub max_chars: u32,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub multiline: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "kebab-case", deny_unknown_fields)]
pub enum DialogProgressValueV1 {
    Indeterminate {},
    Determinate { current: u64, total: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialogFileFilterV1 {
    pub label: String,
    pub extensions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum DialogRequestV1 {
    Message {
        common: DialogCommonV1,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<String>,
    },
    Confirm {
        common: DialogCommonV1,
        actions: Vec<DialogActionV1>,
    },
    Prompt {
        common: DialogCommonV1,
        input: DialogTextInputV1,
    },
    Choice {
        common: DialogCommonV1,
        items: Vec<DialogChoiceItemV1>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        initial: Option<String>,
    },
    MultiChoice {
        common: DialogCommonV1,
        items: Vec<DialogChoiceItemV1>,
        #[serde(default)]
        initial: Vec<String>,
    },
    Progress {
        common: DialogCommonV1,
        progress: DialogProgressValueV1,
        #[serde(default)]
        cancellable: bool,
    },
    FileOpen {
        common: DialogCommonV1,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        initial_directory: Option<String>,
        #[serde(default)]
        filters: Vec<DialogFileFilterV1>,
        #[serde(default)]
        multiple: bool,
    },
    FileSave {
        common: DialogCommonV1,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        initial_directory: Option<String>,
        #[serde(default)]
        filters: Vec<DialogFileFilterV1>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        suggested_name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default_extension: Option<String>,
    },
    DirSelect {
        common: DialogCommonV1,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        initial_directory: Option<String>,
    },
    Slider {
        common: DialogCommonV1,
        min: i32,
        max: i32,
        step: i32,
        initial: i32,
    },
    TextView {
        common: DialogCommonV1,
        text: String,
        #[serde(default)]
        monospace: bool,
    },
}

impl DialogRequestV1 {
    #[must_use]
    pub fn kind(&self) -> DialogKindV1 {
        match self {
            Self::Message { .. } => DialogKindV1::Message,
            Self::Confirm { .. } => DialogKindV1::Confirm,
            Self::Prompt { .. } => DialogKindV1::Prompt,
            Self::Choice { .. } => DialogKindV1::Choice,
            Self::MultiChoice { .. } => DialogKindV1::MultiChoice,
            Self::Progress { .. } => DialogKindV1::Progress,
            Self::FileOpen { .. } => DialogKindV1::FileOpen,
            Self::FileSave { .. } => DialogKindV1::FileSave,
            Self::DirSelect { .. } => DialogKindV1::DirSelect,
            Self::Slider { .. } => DialogKindV1::Slider,
            Self::TextView { .. } => DialogKindV1::TextView,
        }
    }

    pub fn validate(&self) -> Result<(), DialogValidationError> {
        validate_common(self.common())?;
        match self {
            Self::Message { details, .. } => {
                validate_optional_len("details", details.as_deref(), MAX_DIALOG_DETAILS_BYTES)?;
            }
            Self::Confirm { actions, .. } => validate_actions(actions)?,
            Self::Prompt { input, .. } => validate_input(input)?,
            Self::Choice { items, initial, .. } => {
                validate_items(items)?;
                if let Some(key) = initial {
                    validate_initial_key(items, key)?;
                }
            }
            Self::MultiChoice { items, initial, .. } => {
                validate_items(items)?;
                validate_count("initial", initial.len(), MAX_DIALOG_ITEMS)?;
                let mut seen = HashSet::new();
                for key in initial {
                    validate_initial_key(items, key)?;
                    if !seen.insert(key) {
                        return Err(DialogValidationError::DuplicateKey {
                            field: "initial",
                            key: key.clone(),
                        });
                    }
                }
            }
            Self::Progress { progress, .. } => validate_progress(progress)?,
            Self::FileOpen {
                initial_directory,
                filters,
                ..
            } => {
                validate_optional_path("initial_directory", initial_directory.as_deref())?;
                validate_filters(filters, None)?;
            }
            Self::FileSave {
                initial_directory,
                filters,
                suggested_name,
                default_extension,
                ..
            } => {
                validate_optional_path("initial_directory", initial_directory.as_deref())?;
                if let Some(name) = suggested_name {
                    validate_save_name(name)?;
                }
                validate_filters(filters, default_extension.as_deref())?;
            }
            Self::DirSelect {
                initial_directory, ..
            } => validate_optional_path("initial_directory", initial_directory.as_deref())?,
            Self::Slider {
                min,
                max,
                step,
                initial,
                ..
            } => validate_slider(*min, *max, *step, *initial)?,
            Self::TextView { text, .. } => {
                validate_non_empty_len("text", text, MAX_DIALOG_TEXT_VIEW_BYTES)?;
            }
        }
        let got = self.retained_bytes();
        if got > MAX_DIALOG_REQUEST_BYTES {
            return Err(DialogValidationError::RequestTooLarge {
                got,
                max: MAX_DIALOG_REQUEST_BYTES,
            });
        }
        Ok(())
    }

    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        let mut bytes =
            self.common().title.len() + self.common().message.as_ref().map_or(0, String::len);
        match self {
            Self::Message { details, .. } => {
                bytes += details.as_ref().map_or(0, String::len);
            }
            Self::Confirm { actions, .. } => {
                for action in actions {
                    bytes += action.key.len() + action.label.len();
                }
            }
            Self::Prompt { input, .. } => {
                bytes += input.initial.as_ref().map_or(0, String::len);
            }
            Self::Choice { items, initial, .. } => {
                bytes += retained_item_bytes(items) + initial.as_ref().map_or(0, String::len);
            }
            Self::MultiChoice { items, initial, .. } => {
                bytes += retained_item_bytes(items);
                bytes += initial.iter().map(String::len).sum::<usize>();
            }
            Self::Progress { .. } | Self::Slider { .. } => {}
            Self::FileOpen {
                initial_directory,
                filters,
                ..
            }
            | Self::FileSave {
                initial_directory,
                filters,
                ..
            } => {
                bytes += initial_directory.as_ref().map_or(0, String::len);
                bytes += retained_filter_bytes(filters);
                if let Self::FileSave {
                    suggested_name,
                    default_extension,
                    ..
                } = self
                {
                    bytes += suggested_name.as_ref().map_or(0, String::len);
                    bytes += default_extension.as_ref().map_or(0, String::len);
                }
            }
            Self::DirSelect {
                initial_directory, ..
            } => bytes += initial_directory.as_ref().map_or(0, String::len),
            Self::TextView { text, .. } => bytes += text.len(),
        }
        bytes
    }

    fn common(&self) -> &DialogCommonV1 {
        match self {
            Self::Message { common, .. }
            | Self::Confirm { common, .. }
            | Self::Prompt { common, .. }
            | Self::Choice { common, .. }
            | Self::MultiChoice { common, .. }
            | Self::Progress { common, .. }
            | Self::FileOpen { common, .. }
            | Self::FileSave { common, .. }
            | Self::DirSelect { common, .. }
            | Self::Slider { common, .. }
            | Self::TextView { common, .. } => common,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialogOpenRequestV1 {
    pub dialog: DialogRequestV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<u64>,
}

impl DialogOpenRequestV1 {
    pub fn validate(&self) -> Result<(), DialogValidationError> {
        self.dialog.validate()?;
        if self
            .deadline_ms
            .is_some_and(|deadline| deadline < MIN_DIALOG_DEADLINE_MS)
        {
            return Err(DialogValidationError::InvalidDeadline);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub enum DialogStateV1 {
    Queued,
    Presenting,
    Presented,
    CancelRequested,
    Resolved,
    Cancelled,
    Expired,
    Failed,
}

impl DialogStateV1 {
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Resolved | Self::Cancelled | Self::Expired | Self::Failed
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialogOpenResponse {
    pub handle: InteractionHandle,
    pub owner_token: OwnerToken,
    pub state: DialogStateV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialogProgressPatchV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<DialogProgressValueV1>,
}

impl DialogProgressPatchV1 {
    pub fn validate(&self) -> Result<(), DialogValidationError> {
        validate_optional_len("message", self.message.as_deref(), MAX_DIALOG_MESSAGE_BYTES)?;
        if let Some(progress) = &self.progress {
            validate_progress(progress)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialogProgressUpdateRequest {
    pub handle: InteractionHandle,
    pub owner_token: OwnerToken,
    pub patch: DialogProgressPatchV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "kebab-case", deny_unknown_fields)]
pub enum DialogProgressCompletionV1 {
    Succeeded {},
    Cancelled {},
    Failed { message: String },
}

impl DialogProgressCompletionV1 {
    pub fn validate(&self) -> Result<(), DialogValidationError> {
        if let Self::Failed { message } = self {
            validate_non_empty_len("message", message, MAX_DIALOG_MESSAGE_BYTES)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialogProgressCompleteRequest {
    pub handle: InteractionHandle,
    pub owner_token: OwnerToken,
    pub completion: DialogProgressCompletionV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialogCancelRequest {
    pub handle: InteractionHandle,
    pub owner_token: OwnerToken,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialogResultRequest {
    pub handle: InteractionHandle,
    pub owner_token: OwnerToken,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum DialogValueV1 {
    Message {},
    Confirm {
        action: String,
    },
    Prompt {
        text: String,
    },
    Choice {
        key: String,
    },
    MultiChoice {
        keys: Vec<String>,
    },
    Progress {
        completion: DialogProgressCompletionV1,
    },
    FileOpen {
        paths: Vec<String>,
    },
    FileSave {
        path: String,
    },
    DirSelect {
        path: String,
    },
    Slider {
        value: i32,
    },
    TextView {},
}

impl DialogValueV1 {
    pub fn validate_for(&self, request: &DialogRequestV1) -> Result<(), DialogValidationError> {
        match (self, request) {
            (Self::Message {}, DialogRequestV1::Message { .. })
            | (Self::Prompt { .. }, DialogRequestV1::Prompt { .. })
            | (Self::Progress { .. }, DialogRequestV1::Progress { .. })
            | (Self::FileOpen { .. }, DialogRequestV1::FileOpen { .. })
            | (Self::FileSave { .. }, DialogRequestV1::FileSave { .. })
            | (Self::DirSelect { .. }, DialogRequestV1::DirSelect { .. })
            | (Self::Slider { .. }, DialogRequestV1::Slider { .. })
            | (Self::TextView {}, DialogRequestV1::TextView { .. }) => {}
            (Self::Confirm { action }, DialogRequestV1::Confirm { actions, .. }) => {
                if !actions.iter().any(|candidate| candidate.key == *action) {
                    return Err(DialogValidationError::UnknownResultKey(action.clone()));
                }
            }
            (Self::Choice { key }, DialogRequestV1::Choice { items, .. }) => {
                validate_result_choice(items, key)?;
            }
            (Self::MultiChoice { keys }, DialogRequestV1::MultiChoice { items, .. }) => {
                validate_count("keys", keys.len(), MAX_DIALOG_ITEMS)?;
                let mut seen = HashSet::new();
                for key in keys {
                    validate_result_choice(items, key)?;
                    if !seen.insert(key) {
                        return Err(DialogValidationError::DuplicateResultKey(key.clone()));
                    }
                }
            }
            _ => return Err(DialogValidationError::ResultKindMismatch),
        }

        match self {
            Self::Prompt { text } => {
                validate_len("text", text, MAX_DIALOG_PROMPT_INITIAL_BYTES)?;
                if let DialogRequestV1::Prompt { input, .. } = request {
                    if input.required && text.is_empty() {
                        return Err(DialogValidationError::Empty("text"));
                    }
                    if text.chars().count() > input.max_chars as usize {
                        return Err(DialogValidationError::TooLong {
                            field: "text",
                            max: input.max_chars as usize,
                        });
                    }
                }
            }
            Self::Progress { completion } => completion.validate()?,
            Self::FileOpen { paths } => {
                if paths.is_empty() {
                    return Err(DialogValidationError::Empty("paths"));
                }
                validate_count("paths", paths.len(), MAX_DIALOG_PATHS)?;
                if let DialogRequestV1::FileOpen { multiple, .. } = request
                    && !multiple
                    && paths.len() != 1
                {
                    return Err(DialogValidationError::TooMany {
                        field: "paths",
                        max: 1,
                    });
                }
                for path in paths {
                    validate_non_empty_len("path", path, MAX_DIALOG_PATH_BYTES)?;
                }
            }
            Self::FileSave { path } | Self::DirSelect { path } => {
                validate_non_empty_len("path", path, MAX_DIALOG_PATH_BYTES)?;
            }
            Self::Slider { value } => {
                if let DialogRequestV1::Slider { min, max, step, .. } = request {
                    validate_slider(*min, *max, *step, *value)?;
                }
            }
            _ => {}
        }
        let got = self.retained_bytes();
        if got > MAX_DIALOG_RESULT_BYTES {
            return Err(DialogValidationError::ResultTooLarge {
                got,
                max: MAX_DIALOG_RESULT_BYTES,
            });
        }
        Ok(())
    }

    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        match self {
            Self::Message {} | Self::TextView {} | Self::Slider { .. } => 0,
            Self::Confirm { action } => action.len(),
            Self::Prompt { text } => text.len(),
            Self::Choice { key } => key.len(),
            Self::MultiChoice { keys } => keys.iter().map(String::len).sum(),
            Self::Progress { completion } => match completion {
                DialogProgressCompletionV1::Succeeded {}
                | DialogProgressCompletionV1::Cancelled {} => 0,
                DialogProgressCompletionV1::Failed { message } => message.len(),
            },
            Self::FileOpen { paths } => paths.iter().map(String::len).sum(),
            Self::FileSave { path } | Self::DirSelect { path } => path.len(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialogResultResponseV1 {
    pub handle: InteractionHandle,
    pub state: DialogStateV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<DialogValueV1>,
}

/// Echoable wire coordinate for a broker-minted presenter lease.
///
/// Presenters must round-trip all fields verbatim. This DTO cannot be converted
/// into the broker's private `PresenterLease`; interactd retains and validates
/// the real broker-minted lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialogPresenterLeaseV1 {
    pub presenter_service: String,
    pub generation: u64,
    pub instance_epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialogPresenterRegisterRequestV1 {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialogPresenterRegisterResponseV1 {
    pub lease: DialogPresenterLeaseV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialogPresenterReleaseRequestV1 {
    pub lease: DialogPresenterLeaseV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialogPresenterNextRequestV1 {
    pub lease: DialogPresenterLeaseV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialogPresenterNextResponseV1 {
    pub presentation: Option<DialogPresentationV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialogProgressSnapshotV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub progress: DialogProgressValueV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialogPresentationV1 {
    pub handle: String,
    /// Opaque broker-minted coordinate which the presenter only echoes.
    pub attempt_token: u64,
    pub dialog: DialogRequestV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<DialogProgressSnapshotV1>,
    pub cancel_requested: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialogPresenterMarkPresentedRequestV1 {
    pub lease: DialogPresenterLeaseV1,
    pub handle: String,
    pub attempt_token: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialogPresenterResolveRequestV1 {
    pub lease: DialogPresenterLeaseV1,
    pub handle: String,
    pub attempt_token: u64,
    pub value: DialogValueV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialogPresenterFailRequestV1 {
    pub lease: DialogPresenterLeaseV1,
    pub handle: String,
    pub attempt_token: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialogPresenterProgressCancelRequestV1 {
    pub lease: DialogPresenterLeaseV1,
    pub handle: String,
    pub attempt_token: u64,
}

fn default_true() -> bool {
    true
}

fn default_prompt_chars() -> u32 {
    MAX_DIALOG_PROMPT_CHARS
}

fn validate_common(common: &DialogCommonV1) -> Result<(), DialogValidationError> {
    validate_non_empty_len("title", &common.title, MAX_DIALOG_TITLE_BYTES)?;
    validate_optional_len(
        "message",
        common.message.as_deref(),
        MAX_DIALOG_MESSAGE_BYTES,
    )
}

fn validate_actions(actions: &[DialogActionV1]) -> Result<(), DialogValidationError> {
    validate_count("actions", actions.len(), MAX_DIALOG_ACTIONS)?;
    if actions.is_empty() {
        return Err(DialogValidationError::Empty("actions"));
    }
    let mut keys = HashSet::new();
    let mut defaults = 0;
    let mut cancels = 0;
    for action in actions {
        validate_key("actions", &action.key)?;
        validate_non_empty_len("action.label", &action.label, MAX_DIALOG_LABEL_BYTES)?;
        if !keys.insert(&action.key) {
            return Err(DialogValidationError::DuplicateKey {
                field: "actions",
                key: action.key.clone(),
            });
        }
        if action.is_default {
            defaults += 1;
            if action.role == DialogActionRoleV1::Destructive {
                return Err(DialogValidationError::DestructiveDefault);
            }
        }
        if action.role == DialogActionRoleV1::Cancel {
            cancels += 1;
        }
    }
    if defaults > 1 {
        return Err(DialogValidationError::MultipleDefaults);
    }
    if cancels > 1 {
        return Err(DialogValidationError::MultipleCancelActions);
    }
    Ok(())
}

fn validate_input(input: &DialogTextInputV1) -> Result<(), DialogValidationError> {
    if input.max_chars == 0 || input.max_chars > MAX_DIALOG_PROMPT_CHARS {
        return Err(DialogValidationError::TooLong {
            field: "max_chars",
            max: MAX_DIALOG_PROMPT_CHARS as usize,
        });
    }
    if let Some(initial) = &input.initial {
        validate_len("input.initial", initial, MAX_DIALOG_PROMPT_INITIAL_BYTES)?;
        if initial.chars().count() > input.max_chars as usize {
            return Err(DialogValidationError::TooLong {
                field: "input.initial",
                max: input.max_chars as usize,
            });
        }
    }
    Ok(())
}

fn validate_items(items: &[DialogChoiceItemV1]) -> Result<(), DialogValidationError> {
    validate_count("items", items.len(), MAX_DIALOG_ITEMS)?;
    if items.is_empty() {
        return Err(DialogValidationError::Empty("items"));
    }
    let mut keys = HashSet::new();
    for item in items {
        validate_key("items", &item.key)?;
        validate_non_empty_len("item.label", &item.label, MAX_DIALOG_LABEL_BYTES)?;
        validate_optional_len(
            "item.description",
            item.description.as_deref(),
            MAX_DIALOG_MESSAGE_BYTES,
        )?;
        if !keys.insert(&item.key) {
            return Err(DialogValidationError::DuplicateKey {
                field: "items",
                key: item.key.clone(),
            });
        }
    }
    Ok(())
}

fn validate_initial_key(
    items: &[DialogChoiceItemV1],
    key: &str,
) -> Result<(), DialogValidationError> {
    let Some(item) = items.iter().find(|item| item.key == key) else {
        return Err(DialogValidationError::UnknownDefaultKey(key.to_owned()));
    };
    if !item.enabled {
        return Err(DialogValidationError::DisabledDefaultKey(key.to_owned()));
    }
    Ok(())
}

fn validate_result_choice(
    items: &[DialogChoiceItemV1],
    key: &str,
) -> Result<(), DialogValidationError> {
    let Some(item) = items.iter().find(|item| item.key == key) else {
        return Err(DialogValidationError::UnknownResultKey(key.to_owned()));
    };
    if !item.enabled {
        return Err(DialogValidationError::DisabledResultKey(key.to_owned()));
    }
    Ok(())
}

fn validate_progress(progress: &DialogProgressValueV1) -> Result<(), DialogValidationError> {
    if let DialogProgressValueV1::Determinate { current, total } = progress
        && (*total == 0 || current > total)
    {
        return Err(DialogValidationError::InvalidProgress);
    }
    Ok(())
}

fn validate_slider(min: i32, max: i32, step: i32, value: i32) -> Result<(), DialogValidationError> {
    if min >= max || step <= 0 || value < min || value > max {
        return Err(DialogValidationError::InvalidSlider);
    }
    if (i64::from(value) - i64::from(min)) % i64::from(step) != 0 {
        return Err(DialogValidationError::InvalidSlider);
    }
    Ok(())
}

fn validate_save_name(name: &str) -> Result<(), DialogValidationError> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\\') {
        return Err(DialogValidationError::InvalidSaveName);
    }
    validate_len("suggested_name", name, MAX_DIALOG_PATH_BYTES)
}

fn validate_filters(
    filters: &[DialogFileFilterV1],
    default_extension: Option<&str>,
) -> Result<(), DialogValidationError> {
    validate_count("filters", filters.len(), MAX_DIALOG_FILTERS)?;
    let mut all_extensions = HashSet::new();
    for filter in filters {
        validate_non_empty_len("filter.label", &filter.label, MAX_DIALOG_LABEL_BYTES)?;
        validate_count(
            "filter.extensions",
            filter.extensions.len(),
            MAX_DIALOG_EXTENSIONS_PER_FILTER,
        )?;
        if filter.extensions.is_empty() {
            return Err(DialogValidationError::Empty("filter.extensions"));
        }
        let mut filter_extensions = HashSet::new();
        for extension in &filter.extensions {
            validate_extension(extension)?;
            if !filter_extensions.insert(extension) {
                return Err(DialogValidationError::DuplicateKey {
                    field: "filter.extensions",
                    key: extension.clone(),
                });
            }
            all_extensions.insert(extension.as_str());
        }
    }
    if let Some(extension) = default_extension {
        validate_extension(extension)?;
        if !all_extensions.contains(extension) {
            return Err(DialogValidationError::UnknownDefaultKey(
                extension.to_owned(),
            ));
        }
    }
    Ok(())
}

fn validate_extension(extension: &str) -> Result<(), DialogValidationError> {
    if extension.is_empty()
        || extension.starts_with('.')
        || !extension.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'+' | b'-')
        })
    {
        return Err(DialogValidationError::InvalidExtension(
            extension.to_owned(),
        ));
    }
    Ok(())
}

fn validate_optional_path(
    field: &'static str,
    value: Option<&str>,
) -> Result<(), DialogValidationError> {
    if let Some(value) = value {
        validate_non_empty_len(field, value, MAX_DIALOG_PATH_BYTES)?;
    }
    Ok(())
}

fn validate_key(field: &'static str, key: &str) -> Result<(), DialogValidationError> {
    if key.is_empty()
        || key.len() > MAX_DIALOG_KEY_BYTES
        || !key.is_ascii()
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(DialogValidationError::InvalidKey {
            field,
            key: key.to_owned(),
        });
    }
    Ok(())
}

fn validate_non_empty_len(
    field: &'static str,
    value: &str,
    max: usize,
) -> Result<(), DialogValidationError> {
    if value.is_empty() {
        return Err(DialogValidationError::Empty(field));
    }
    validate_len(field, value, max)
}

fn validate_optional_len(
    field: &'static str,
    value: Option<&str>,
    max: usize,
) -> Result<(), DialogValidationError> {
    if let Some(value) = value {
        validate_len(field, value, max)?;
    }
    Ok(())
}

fn validate_len(field: &'static str, value: &str, max: usize) -> Result<(), DialogValidationError> {
    if value.len() > max {
        return Err(DialogValidationError::TooLong { field, max });
    }
    Ok(())
}

fn validate_count(
    field: &'static str,
    actual: usize,
    max: usize,
) -> Result<(), DialogValidationError> {
    if actual > max {
        return Err(DialogValidationError::TooMany { field, max });
    }
    Ok(())
}

fn retained_item_bytes(items: &[DialogChoiceItemV1]) -> usize {
    items
        .iter()
        .map(|item| {
            item.key.len() + item.label.len() + item.description.as_ref().map_or(0, String::len)
        })
        .sum()
}

fn retained_filter_bytes(filters: &[DialogFileFilterV1]) -> usize {
    filters
        .iter()
        .map(|filter| filter.label.len() + filter.extensions.iter().map(String::len).sum::<usize>())
        .sum()
}

#[cfg(test)]
mod tests {
    use serde::Serialize;
    use serde::de::DeserializeOwned;
    use serde_json::{Value, json};

    use super::*;

    fn common() -> DialogCommonV1 {
        DialogCommonV1 {
            title: "Example".into(),
            message: Some("Choose carefully".into()),
            severity: DialogSeverityV1::Info,
        }
    }

    fn item(key: &str) -> DialogChoiceItemV1 {
        DialogChoiceItemV1 {
            key: key.into(),
            label: key.into(),
            description: None,
            enabled: true,
        }
    }

    fn action(key: &str, role: DialogActionRoleV1) -> DialogActionV1 {
        DialogActionV1 {
            key: key.into(),
            label: key.into(),
            role,
            is_default: false,
        }
    }

    fn requests() -> Vec<DialogRequestV1> {
        vec![
            DialogRequestV1::Message {
                common: common(),
                details: Some("Details".into()),
            },
            DialogRequestV1::Confirm {
                common: common(),
                actions: vec![
                    DialogActionV1 {
                        is_default: true,
                        ..action("yes", DialogActionRoleV1::Accept)
                    },
                    action("no", DialogActionRoleV1::Cancel),
                ],
            },
            DialogRequestV1::Prompt {
                common: common(),
                input: DialogTextInputV1 {
                    initial: Some("hello".into()),
                    max_chars: 32,
                    required: true,
                    multiline: false,
                },
            },
            DialogRequestV1::Choice {
                common: common(),
                items: vec![item("one"), item("two")],
                initial: Some("one".into()),
            },
            DialogRequestV1::MultiChoice {
                common: common(),
                items: vec![item("one"), item("two")],
                initial: vec!["one".into()],
            },
            DialogRequestV1::Progress {
                common: common(),
                progress: DialogProgressValueV1::Determinate {
                    current: 1,
                    total: 2,
                },
                cancellable: true,
            },
            DialogRequestV1::FileOpen {
                common: common(),
                initial_directory: Some("/tmp".into()),
                filters: vec![DialogFileFilterV1 {
                    label: "Text".into(),
                    extensions: vec!["txt".into()],
                }],
                multiple: true,
            },
            DialogRequestV1::FileSave {
                common: common(),
                initial_directory: Some("/tmp".into()),
                filters: vec![DialogFileFilterV1 {
                    label: "Archives".into(),
                    extensions: vec!["tar".into()],
                }],
                suggested_name: Some("backup.tar".into()),
                default_extension: Some("tar".into()),
            },
            DialogRequestV1::DirSelect {
                common: common(),
                initial_directory: Some("/tmp".into()),
            },
            DialogRequestV1::Slider {
                common: common(),
                min: -10,
                max: 10,
                step: 5,
                initial: 0,
            },
            DialogRequestV1::TextView {
                common: common(),
                text: "Read me".into(),
                monospace: true,
            },
        ]
    }

    #[test]
    fn every_v1_request_round_trips_and_validates() {
        for request in requests() {
            request.validate().unwrap();
            let encoded = serde_json::to_string(&request).unwrap();
            assert_eq!(
                serde_json::from_str::<DialogRequestV1>(&encoded).unwrap(),
                request
            );
        }
    }

    #[test]
    fn serde_kind_set_has_no_secret_prompt() {
        let mut tags: Vec<_> = requests()
            .into_iter()
            .map(|request| {
                serde_json::to_value(request)
                    .unwrap()
                    .get("kind")
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        tags.sort();
        assert_eq!(
            tags,
            vec![
                "choice",
                "confirm",
                "dir-select",
                "file-open",
                "file-save",
                "message",
                "multi-choice",
                "progress",
                "prompt",
                "slider",
                "text-view",
            ]
        );
        for forbidden in ["secret", "password", "secret-prompt"] {
            assert!(!tags.contains(&forbidden.to_owned()));
            let value = json!({"kind": forbidden, "title": "No"});
            assert!(serde_json::from_value::<DialogRequestV1>(value).is_err());
        }
    }

    #[test]
    fn unknown_fields_and_unit_result_content_are_rejected() {
        let secret_prompt = json!({
            "kind": "prompt",
            "common": {
                "title": "Password",
                "severity": "info"
            },
            "input": {
                "max_chars": 32,
                "required": false,
                "multiline": false
            },
            "secret": true
        });
        assert!(serde_json::from_value::<DialogRequestV1>(secret_prompt).is_err());

        let unknown_nested = json!({
            "kind": "choice",
            "common": {
                "title": "Choose",
                "severity": "info"
            },
            "items": [{
                "key": "one",
                "label": "One",
                "enabled": true,
                "markup": "<b>One</b>"
            }]
        });
        assert!(serde_json::from_value::<DialogRequestV1>(unknown_nested).is_err());

        for value in [
            json!({"kind": "message", "text": "smuggled"}),
            json!({"kind": "text-view", "secret": true}),
        ] {
            assert!(serde_json::from_value::<DialogValueV1>(value).is_err());
        }
        assert!(
            serde_json::from_value::<DialogProgressCompletionV1>(
                json!({"outcome": "succeeded", "detail": "smuggled"})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<DialogProgressValueV1>(
                json!({"mode": "indeterminate", "current": 1})
            )
            .is_err()
        );
    }

    #[test]
    fn raw_ingress_cap_is_a_named_transport_contract() {
        assert_eq!(DIALOG_RAW_INGRESS_CAP, 16 * 1024);
        assert_eq!(MAX_DIALOG_REQUEST_BYTES, DIALOG_RAW_INGRESS_CAP);
        assert_eq!(MAX_DIALOG_RESULT_BYTES, DIALOG_RAW_INGRESS_CAP);
    }

    #[test]
    fn interaction_handle_keeps_plain_string_json() {
        assert_eq!(
            serde_json::to_value(InteractionHandle("n-existing".into())).unwrap(),
            Value::String("n-existing".into())
        );
        assert_eq!(
            serde_json::from_value::<InteractionHandle>(json!("d-new")).unwrap(),
            InteractionHandle("d-new".into())
        );
        let legacy = crate::NotifyHandle("n-legacy".into());
        assert_eq!(legacy.as_str(), "n-legacy");
    }

    #[test]
    fn dialog_envelopes_round_trip() {
        let open = DialogOpenRequestV1 {
            dialog: requests().remove(0),
            deadline_ms: Some(MIN_DIALOG_DEADLINE_MS),
        };
        open.validate().unwrap();
        let response = DialogOpenResponse {
            handle: InteractionHandle("d1".into()),
            owner_token: OwnerToken("owner".into()),
            state: DialogStateV1::Queued,
        };
        let update = DialogProgressUpdateRequest {
            handle: response.handle.clone(),
            owner_token: response.owner_token.clone(),
            patch: DialogProgressPatchV1 {
                message: Some("Halfway".into()),
                progress: Some(DialogProgressValueV1::Determinate {
                    current: 1,
                    total: 2,
                }),
            },
        };
        let complete = DialogProgressCompleteRequest {
            handle: response.handle.clone(),
            owner_token: response.owner_token.clone(),
            completion: DialogProgressCompletionV1::Succeeded {},
        };
        let cancel = DialogCancelRequest {
            handle: response.handle.clone(),
            owner_token: response.owner_token.clone(),
        };
        let result = DialogResultRequest {
            handle: response.handle.clone(),
            owner_token: response.owner_token.clone(),
        };
        for value in [
            serde_json::to_value(open).unwrap(),
            serde_json::to_value(response).unwrap(),
            serde_json::to_value(update).unwrap(),
            serde_json::to_value(complete).unwrap(),
            serde_json::to_value(cancel).unwrap(),
            serde_json::to_value(result).unwrap(),
        ] {
            assert!(value.is_object());
        }
    }

    fn assert_round_trip<T>(value: &T)
    where
        T: Clone + std::fmt::Debug + PartialEq + Serialize + DeserializeOwned,
    {
        let encoded = serde_json::to_vec(value).unwrap();
        assert_eq!(serde_json::from_slice::<T>(&encoded).unwrap(), *value);
    }

    fn assert_unknown_field_rejected<T>(value: &T)
    where
        T: Serialize + DeserializeOwned,
    {
        let mut encoded = serde_json::to_value(value).unwrap();
        encoded
            .as_object_mut()
            .expect("presenter DTOs are JSON objects")
            .insert("unexpected".into(), json!(true));
        assert!(serde_json::from_value::<T>(encoded).is_err());
    }

    #[test]
    fn presenter_wire_dtos_round_trip_and_reject_unknown_fields() {
        let lease = DialogPresenterLeaseV1 {
            presenter_service: "interact-gui".into(),
            generation: 7,
            instance_epoch: 99,
        };
        let register = DialogPresenterRegisterRequestV1 {};
        let register_response = DialogPresenterRegisterResponseV1 {
            lease: lease.clone(),
        };
        let release = DialogPresenterReleaseRequestV1 {
            lease: lease.clone(),
        };
        let next = DialogPresenterNextRequestV1 {
            lease: lease.clone(),
        };
        let progress = DialogProgressSnapshotV1 {
            message: Some("Uploading".into()),
            progress: DialogProgressValueV1::Determinate {
                current: 1,
                total: 2,
            },
        };
        let presentation = DialogPresentationV1 {
            handle: "d0123456789abcdef".into(),
            attempt_token: 42,
            dialog: requests().remove(0),
            progress: Some(progress.clone()),
            cancel_requested: false,
        };
        let next_response = DialogPresenterNextResponseV1 {
            presentation: Some(presentation.clone()),
        };
        let mark = DialogPresenterMarkPresentedRequestV1 {
            lease: lease.clone(),
            handle: presentation.handle.clone(),
            attempt_token: presentation.attempt_token,
        };
        let resolve = DialogPresenterResolveRequestV1 {
            lease: lease.clone(),
            handle: presentation.handle.clone(),
            attempt_token: presentation.attempt_token,
            value: DialogValueV1::Message {},
        };
        let fail = DialogPresenterFailRequestV1 {
            lease: lease.clone(),
            handle: presentation.handle.clone(),
            attempt_token: presentation.attempt_token,
        };
        let progress_cancel = DialogPresenterProgressCancelRequestV1 {
            lease: lease.clone(),
            handle: presentation.handle.clone(),
            attempt_token: presentation.attempt_token,
        };

        assert_round_trip(&lease);
        assert_round_trip(&register);
        assert_round_trip(&register_response);
        assert_round_trip(&release);
        assert_round_trip(&next);
        assert_round_trip(&progress);
        assert_round_trip(&presentation);
        assert_round_trip(&next_response);
        assert_round_trip(&mark);
        assert_round_trip(&resolve);
        assert_round_trip(&fail);
        assert_round_trip(&progress_cancel);

        assert_unknown_field_rejected(&lease);
        assert_unknown_field_rejected(&register);
        assert_unknown_field_rejected(&register_response);
        assert_unknown_field_rejected(&release);
        assert_unknown_field_rejected(&next);
        assert_unknown_field_rejected(&progress);
        assert_unknown_field_rejected(&presentation);
        assert_unknown_field_rejected(&next_response);
        assert_unknown_field_rejected(&mark);
        assert_unknown_field_rejected(&resolve);
        assert_unknown_field_rejected(&fail);
        assert_unknown_field_rejected(&progress_cancel);

        let presenter_json = serde_json::to_string(&next_response).unwrap();
        assert!(!presenter_json.contains("owner_token"));
    }

    #[test]
    fn presenter_register_request_cannot_assert_identity() {
        let register = DialogPresenterRegisterRequestV1 {};
        assert_eq!(serde_json::to_value(&register).unwrap(), json!({}));
        assert!(
            serde_json::from_value::<DialogPresenterRegisterRequestV1>(
                json!({"presenter_service": "interact-gui"})
            )
            .is_err()
        );
    }

    #[test]
    fn presenter_lease_echo_is_byte_identical() {
        let lease = DialogPresenterLeaseV1 {
            presenter_service: "interact-gui".into(),
            generation: u64::MAX - 1,
            instance_epoch: u64::MAX,
        };
        let encoded = serde_json::to_vec(&lease).unwrap();
        let decoded: DialogPresenterLeaseV1 = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(serde_json::to_vec(&decoded).unwrap(), encoded);
    }

    #[test]
    fn dialog_wire_names_are_fixed() {
        assert_eq!(VERB_DIALOG_OPEN, "interact.dialog-open");
        assert_eq!(
            VERB_DIALOG_PROGRESS_UPDATE,
            "interact.dialog-progress-update"
        );
        assert_eq!(
            VERB_DIALOG_PROGRESS_COMPLETE,
            "interact.dialog-progress-complete"
        );
        assert_eq!(VERB_DIALOG_CANCEL, "interact.dialog-cancel");
        assert_eq!(VERB_DIALOG_RESULT, "interact.dialog-result");
        assert_eq!(VERB_PRESENTER_REGISTER, "interact.presenter-register");
        assert_eq!(VERB_PRESENTER_RELEASE, "interact.presenter-release");
        assert_eq!(VERB_PRESENTER_NEXT, "interact.presenter-next");
        assert_eq!(
            VERB_PRESENTER_MARK_PRESENTED,
            "interact.presenter-mark-presented"
        );
        assert_eq!(VERB_PRESENTER_RESOLVE, "interact.presenter-resolve");
        assert_eq!(VERB_PRESENTER_FAIL, "interact.presenter-fail");
        assert_eq!(
            VERB_PRESENTER_PROGRESS_CANCEL,
            "interact.presenter-progress-cancel"
        );
        assert_eq!(TOPIC_INTERACT_PROPS_CHANGED, "interact.props.changed");
    }

    #[test]
    fn deadline_boundary_is_inclusive() {
        let mut open = DialogOpenRequestV1 {
            dialog: requests().remove(0),
            deadline_ms: Some(MIN_DIALOG_DEADLINE_MS),
        };
        open.validate().unwrap();
        open.deadline_ms = Some(MIN_DIALOG_DEADLINE_MS - 1);
        assert_eq!(open.validate(), Err(DialogValidationError::InvalidDeadline));
        open.deadline_ms = None;
        open.validate().unwrap();
    }

    #[test]
    fn common_and_content_byte_caps_are_enforced() {
        let mut request = DialogRequestV1::Message {
            common: DialogCommonV1 {
                title: "t".repeat(MAX_DIALOG_TITLE_BYTES),
                message: None,
                severity: DialogSeverityV1::Info,
            },
            details: None,
        };
        request.validate().unwrap();
        if let DialogRequestV1::Message { common, .. } = &mut request {
            common.title.push('x');
        }
        assert!(matches!(
            request.validate(),
            Err(DialogValidationError::TooLong { field: "title", .. })
        ));

        let message = "m".repeat(MAX_DIALOG_MESSAGE_BYTES);
        DialogRequestV1::Message {
            common: DialogCommonV1 {
                title: "t".into(),
                message: Some(message.clone()),
                severity: DialogSeverityV1::Info,
            },
            details: None,
        }
        .validate()
        .unwrap();
        assert!(
            DialogRequestV1::Message {
                common: DialogCommonV1 {
                    title: "t".into(),
                    message: Some(format!("{message}x")),
                    severity: DialogSeverityV1::Info,
                },
                details: None,
            }
            .validate()
            .is_err()
        );

        let details = "d".repeat(MAX_DIALOG_DETAILS_BYTES);
        DialogRequestV1::Message {
            common: DialogCommonV1 {
                title: "t".into(),
                message: None,
                severity: DialogSeverityV1::Info,
            },
            details: Some(details.clone()),
        }
        .validate()
        .unwrap();
        assert!(
            DialogRequestV1::Message {
                common: DialogCommonV1 {
                    title: "t".into(),
                    message: None,
                    severity: DialogSeverityV1::Info,
                },
                details: Some(format!("{details}x")),
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn text_view_and_aggregate_caps_are_enforced() {
        let text = "x".repeat(MAX_DIALOG_TEXT_VIEW_BYTES);
        DialogRequestV1::TextView {
            common: DialogCommonV1 {
                title: "t".into(),
                message: None,
                severity: DialogSeverityV1::Info,
            },
            text: text.clone(),
            monospace: false,
        }
        .validate()
        .unwrap();
        assert!(
            DialogRequestV1::TextView {
                common: DialogCommonV1 {
                    title: "t".into(),
                    message: None,
                    severity: DialogSeverityV1::Info,
                },
                text: format!("{text}x"),
                monospace: false,
            }
            .validate()
            .is_err()
        );

        let huge_description = "x".repeat(MAX_DIALOG_MESSAGE_BYTES);
        let aggregate = DialogRequestV1::Choice {
            common: DialogCommonV1 {
                title: "t".into(),
                message: None,
                severity: DialogSeverityV1::Info,
            },
            items: vec![
                DialogChoiceItemV1 {
                    description: Some(huge_description.clone()),
                    ..item("one")
                },
                DialogChoiceItemV1 {
                    description: Some(huge_description),
                    ..item("two")
                },
            ],
            initial: None,
        };
        assert!(matches!(
            aggregate.validate(),
            Err(DialogValidationError::RequestTooLarge { .. })
        ));
    }

    #[test]
    fn action_keys_counts_and_roles_are_validated() {
        let actions: Vec<_> = (0..MAX_DIALOG_ACTIONS)
            .map(|index| action(&format!("a{index}"), DialogActionRoleV1::Accept))
            .collect();
        DialogRequestV1::Confirm {
            common: common(),
            actions: actions.clone(),
        }
        .validate()
        .unwrap();
        let mut too_many = actions;
        too_many.push(action("extra", DialogActionRoleV1::Accept));
        assert!(matches!(
            DialogRequestV1::Confirm {
                common: common(),
                actions: too_many
            }
            .validate(),
            Err(DialogValidationError::TooMany {
                field: "actions",
                ..
            })
        ));

        for invalid in ["", "space key", "é"] {
            assert!(
                DialogRequestV1::Confirm {
                    common: common(),
                    actions: vec![action(invalid, DialogActionRoleV1::Accept)],
                }
                .validate()
                .is_err()
            );
        }
        assert!(
            DialogRequestV1::Confirm {
                common: common(),
                actions: vec![
                    action("same", DialogActionRoleV1::Accept),
                    action("same", DialogActionRoleV1::Cancel),
                ],
            }
            .validate()
            .is_err()
        );
        assert_eq!(
            DialogRequestV1::Confirm {
                common: common(),
                actions: vec![DialogActionV1 {
                    is_default: true,
                    ..action("delete", DialogActionRoleV1::Destructive)
                }],
            }
            .validate(),
            Err(DialogValidationError::DestructiveDefault)
        );
        assert_eq!(
            DialogRequestV1::Confirm {
                common: common(),
                actions: vec![
                    DialogActionV1 {
                        is_default: true,
                        ..action("one", DialogActionRoleV1::Accept)
                    },
                    DialogActionV1 {
                        is_default: true,
                        ..action("two", DialogActionRoleV1::Accept)
                    },
                ],
            }
            .validate(),
            Err(DialogValidationError::MultipleDefaults)
        );
        assert_eq!(
            DialogRequestV1::Confirm {
                common: common(),
                actions: vec![
                    action("one", DialogActionRoleV1::Cancel),
                    action("two", DialogActionRoleV1::Cancel),
                ],
            }
            .validate(),
            Err(DialogValidationError::MultipleCancelActions)
        );
    }

    #[test]
    fn choice_item_counts_defaults_and_uniqueness_are_validated() {
        let items: Vec<_> = (0..MAX_DIALOG_ITEMS)
            .map(|index| item(&format!("i{index}")))
            .collect();
        DialogRequestV1::Choice {
            common: common(),
            items: items.clone(),
            initial: Some("i0".into()),
        }
        .validate()
        .unwrap();
        let mut too_many = items;
        too_many.push(item("extra"));
        assert!(
            DialogRequestV1::Choice {
                common: common(),
                items: too_many,
                initial: None,
            }
            .validate()
            .is_err()
        );

        let disabled = DialogChoiceItemV1 {
            enabled: false,
            ..item("off")
        };
        assert_eq!(
            DialogRequestV1::Choice {
                common: common(),
                items: vec![disabled.clone()],
                initial: Some("off".into()),
            }
            .validate(),
            Err(DialogValidationError::DisabledDefaultKey("off".into()))
        );
        assert_eq!(
            DialogRequestV1::Choice {
                common: common(),
                items: vec![item("one")],
                initial: Some("missing".into()),
            }
            .validate(),
            Err(DialogValidationError::UnknownDefaultKey("missing".into()))
        );
        assert!(
            DialogRequestV1::Choice {
                common: common(),
                items: vec![item("same"), item("same")],
                initial: None,
            }
            .validate()
            .is_err()
        );
        assert!(
            DialogRequestV1::MultiChoice {
                common: common(),
                items: vec![item("one")],
                initial: vec!["one".into(), "one".into()],
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn progress_boundaries_are_validated() {
        for progress in [
            DialogProgressValueV1::Indeterminate {},
            DialogProgressValueV1::Determinate {
                current: 0,
                total: 1,
            },
            DialogProgressValueV1::Determinate {
                current: 1,
                total: 1,
            },
        ] {
            DialogRequestV1::Progress {
                common: common(),
                progress,
                cancellable: true,
            }
            .validate()
            .unwrap();
        }
        for progress in [
            DialogProgressValueV1::Determinate {
                current: 0,
                total: 0,
            },
            DialogProgressValueV1::Determinate {
                current: 2,
                total: 1,
            },
        ] {
            assert_eq!(
                DialogRequestV1::Progress {
                    common: common(),
                    progress,
                    cancellable: true,
                }
                .validate(),
                Err(DialogValidationError::InvalidProgress)
            );
        }
    }

    #[test]
    fn prompt_limits_are_in_chars_and_bytes() {
        DialogRequestV1::Prompt {
            common: common(),
            input: DialogTextInputV1 {
                initial: Some("é".repeat(4)),
                max_chars: 4,
                required: false,
                multiline: false,
            },
        }
        .validate()
        .unwrap();
        assert!(
            DialogRequestV1::Prompt {
                common: common(),
                input: DialogTextInputV1 {
                    initial: None,
                    max_chars: 0,
                    required: false,
                    multiline: false,
                },
            }
            .validate()
            .is_err()
        );
        assert!(
            DialogRequestV1::Prompt {
                common: common(),
                input: DialogTextInputV1 {
                    initial: Some("xxxxx".into()),
                    max_chars: 4,
                    required: false,
                    multiline: false,
                },
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn remaining_field_and_collection_caps_are_inclusive() {
        let max_key = "k".repeat(MAX_DIALOG_KEY_BYTES);
        let max_label = "l".repeat(MAX_DIALOG_LABEL_BYTES);
        DialogRequestV1::Confirm {
            common: common(),
            actions: vec![DialogActionV1 {
                key: max_key.clone(),
                label: max_label.clone(),
                role: DialogActionRoleV1::Accept,
                is_default: true,
            }],
        }
        .validate()
        .unwrap();
        assert!(
            DialogRequestV1::Confirm {
                common: common(),
                actions: vec![DialogActionV1 {
                    key: format!("{max_key}x"),
                    label: "ok".into(),
                    role: DialogActionRoleV1::Accept,
                    is_default: true,
                }],
            }
            .validate()
            .is_err()
        );
        assert!(
            DialogRequestV1::Confirm {
                common: common(),
                actions: vec![DialogActionV1 {
                    key: "ok".into(),
                    label: format!("{max_label}x"),
                    role: DialogActionRoleV1::Accept,
                    is_default: true,
                }],
            }
            .validate()
            .is_err()
        );

        let max_path = "p".repeat(MAX_DIALOG_PATH_BYTES);
        DialogRequestV1::DirSelect {
            common: common(),
            initial_directory: Some(max_path.clone()),
        }
        .validate()
        .unwrap();
        assert!(
            DialogRequestV1::DirSelect {
                common: common(),
                initial_directory: Some(format!("{max_path}x")),
            }
            .validate()
            .is_err()
        );

        let max_prompt_bytes = "é".repeat(MAX_DIALOG_PROMPT_INITIAL_BYTES / 2);
        DialogRequestV1::Prompt {
            common: common(),
            input: DialogTextInputV1 {
                initial: Some(max_prompt_bytes.clone()),
                max_chars: MAX_DIALOG_PROMPT_CHARS,
                required: false,
                multiline: false,
            },
        }
        .validate()
        .unwrap();
        assert!(
            DialogRequestV1::Prompt {
                common: common(),
                input: DialogTextInputV1 {
                    initial: Some(format!("{max_prompt_bytes}é")),
                    max_chars: MAX_DIALOG_PROMPT_CHARS,
                    required: false,
                    multiline: false,
                },
            }
            .validate()
            .is_err()
        );

        let extensions: Vec<_> = (0..MAX_DIALOG_EXTENSIONS_PER_FILTER)
            .map(|index| format!("x{index}"))
            .collect();
        validate_filters(
            &[DialogFileFilterV1 {
                label: max_label.clone(),
                extensions: extensions.clone(),
            }],
            Some("x0"),
        )
        .unwrap();
        let mut too_many_extensions = extensions;
        too_many_extensions.push("extra".into());
        assert!(
            validate_filters(
                &[DialogFileFilterV1 {
                    label: "Files".into(),
                    extensions: too_many_extensions,
                }],
                None,
            )
            .is_err()
        );
        assert!(
            validate_filters(
                &[DialogFileFilterV1 {
                    label: format!("{max_label}x"),
                    extensions: vec!["txt".into()],
                }],
                None,
            )
            .is_err()
        );

        let max_failure = "f".repeat(MAX_DIALOG_MESSAGE_BYTES);
        DialogProgressCompletionV1::Failed {
            message: max_failure.clone(),
        }
        .validate()
        .unwrap();
        assert!(
            DialogProgressCompletionV1::Failed {
                message: format!("{max_failure}x"),
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn slider_boundaries_and_overflow_edges_are_validated() {
        for (min, max, step, value) in [
            (0, 10, 2, 0),
            (0, 10, 2, 10),
            (i32::MIN, i32::MAX, 1, i32::MAX),
        ] {
            validate_slider(min, max, step, value).unwrap();
        }
        for values in [(0, 10, 0, 0), (0, 10, 2, 3), (0, 10, 2, 11), (1, 1, 1, 1)] {
            assert_eq!(
                validate_slider(values.0, values.1, values.2, values.3),
                Err(DialogValidationError::InvalidSlider)
            );
        }
    }

    #[test]
    fn save_names_extensions_and_filter_bounds_are_validated() {
        for name in ["report.txt", "archive"] {
            validate_save_name(name).unwrap();
        }
        for name in ["", ".", "..", "dir/file", r"dir\file"] {
            assert_eq!(
                validate_save_name(name),
                Err(DialogValidationError::InvalidSaveName)
            );
        }
        for extension in ["txt", "7z", "c++", "tar-gz"] {
            validate_extension(extension).unwrap();
        }
        for extension in [".txt", "TXT", "tar.gz", "a/b", ""] {
            assert!(validate_extension(extension).is_err());
        }

        let filters: Vec<_> = (0..MAX_DIALOG_FILTERS)
            .map(|index| DialogFileFilterV1 {
                label: format!("F{index}"),
                extensions: vec![format!("x{index}")],
            })
            .collect();
        validate_filters(&filters, Some("x0")).unwrap();
        let mut too_many = filters;
        too_many.push(DialogFileFilterV1 {
            label: "extra".into(),
            extensions: vec!["extra".into()],
        });
        assert!(validate_filters(&too_many, None).is_err());
        assert!(
            validate_filters(
                &[DialogFileFilterV1 {
                    label: "Text".into(),
                    extensions: vec!["txt".into()],
                }],
                Some("pdf")
            )
            .is_err()
        );
    }

    #[test]
    fn result_variant_and_value_constraints_match_request() {
        let variants = [
            DialogValueV1::Message {},
            DialogValueV1::Confirm {
                action: "yes".into(),
            },
            DialogValueV1::Prompt {
                text: "answer".into(),
            },
            DialogValueV1::Choice { key: "one".into() },
            DialogValueV1::MultiChoice {
                keys: vec!["one".into()],
            },
            DialogValueV1::Progress {
                completion: DialogProgressCompletionV1::Succeeded {},
            },
            DialogValueV1::FileOpen {
                paths: vec!["/tmp/a".into()],
            },
            DialogValueV1::FileSave {
                path: "/tmp/a".into(),
            },
            DialogValueV1::DirSelect {
                path: "/tmp".into(),
            },
            DialogValueV1::Slider { value: 0 },
            DialogValueV1::TextView {},
        ];
        for (value, request) in variants.iter().zip(requests()) {
            value.validate_for(&request).unwrap();
            let encoded = serde_json::to_string(value).unwrap();
            assert_eq!(
                serde_json::from_str::<DialogValueV1>(&encoded).unwrap(),
                *value
            );
        }
        assert_eq!(
            DialogValueV1::Prompt { text: "no".into() }.validate_for(&requests().remove(0)),
            Err(DialogValidationError::ResultKindMismatch)
        );

        let choice = DialogRequestV1::Choice {
            common: common(),
            items: vec![
                item("on"),
                DialogChoiceItemV1 {
                    enabled: false,
                    ..item("off")
                },
            ],
            initial: None,
        };
        assert!(
            DialogValueV1::Choice {
                key: "missing".into()
            }
            .validate_for(&choice)
            .is_err()
        );
        assert!(
            DialogValueV1::Choice { key: "off".into() }
                .validate_for(&choice)
                .is_err()
        );

        let required = DialogRequestV1::Prompt {
            common: common(),
            input: DialogTextInputV1 {
                initial: None,
                max_chars: 2,
                required: true,
                multiline: false,
            },
        };
        assert!(
            DialogValueV1::Prompt {
                text: String::new()
            }
            .validate_for(&required)
            .is_err()
        );
        assert!(
            DialogValueV1::Prompt { text: "abc".into() }
                .validate_for(&required)
                .is_err()
        );
    }

    #[test]
    fn file_and_multi_choice_results_enforce_cardinality() {
        let single_open = DialogRequestV1::FileOpen {
            common: common(),
            initial_directory: None,
            filters: Vec::new(),
            multiple: false,
        };
        assert!(
            DialogValueV1::FileOpen { paths: Vec::new() }
                .validate_for(&single_open)
                .is_err()
        );
        assert!(
            DialogValueV1::FileOpen {
                paths: vec!["a".into(), "b".into()]
            }
            .validate_for(&single_open)
            .is_err()
        );

        let multi = DialogRequestV1::MultiChoice {
            common: common(),
            items: vec![item("one")],
            initial: Vec::new(),
        };
        assert!(
            DialogValueV1::MultiChoice {
                keys: vec!["one".into(), "one".into()]
            }
            .validate_for(&multi)
            .is_err()
        );

        let many_items: Vec<_> = (0..MAX_DIALOG_ITEMS)
            .map(|index| item(&format!("i{index}")))
            .collect();
        let mut many_initial: Vec<_> = many_items.iter().map(|item| item.key.clone()).collect();
        many_initial.push("i0".into());
        assert!(
            DialogRequestV1::MultiChoice {
                common: common(),
                items: many_items.clone(),
                initial: many_initial,
            }
            .validate()
            .is_err()
        );
        assert!(matches!(
            DialogValueV1::MultiChoice {
                keys: (0..=MAX_DIALOG_ITEMS)
                    .map(|index| format!("i{}", index % MAX_DIALOG_ITEMS))
                    .collect(),
            }
            .validate_for(&DialogRequestV1::MultiChoice {
                common: common(),
                items: many_items,
                initial: Vec::new(),
            }),
            Err(DialogValidationError::TooMany { field: "keys", .. })
        ));
    }

    #[test]
    fn multi_file_results_have_count_and_aggregate_caps() {
        let request = DialogRequestV1::FileOpen {
            common: common(),
            initial_directory: None,
            filters: Vec::new(),
            multiple: true,
        };
        let too_many = DialogValueV1::FileOpen {
            paths: (0..=MAX_DIALOG_PATHS)
                .map(|index| format!("/tmp/{index}"))
                .collect(),
        };
        assert!(matches!(
            too_many.validate_for(&request),
            Err(DialogValidationError::TooMany { field: "paths", .. })
        ));

        let aggregate = DialogValueV1::FileOpen {
            paths: (0..5)
                .map(|index| format!("/{index}{}", "x".repeat(MAX_DIALOG_PATH_BYTES - 2)))
                .collect(),
        };
        assert!(matches!(
            aggregate.validate_for(&request),
            Err(DialogValidationError::ResultTooLarge { .. })
        ));
    }

    #[test]
    fn terminal_state_set_is_exact() {
        for state in [
            DialogStateV1::Queued,
            DialogStateV1::Presenting,
            DialogStateV1::Presented,
            DialogStateV1::CancelRequested,
        ] {
            assert!(!state.is_terminal());
        }
        for state in [
            DialogStateV1::Resolved,
            DialogStateV1::Cancelled,
            DialogStateV1::Expired,
            DialogStateV1::Failed,
        ] {
            assert!(state.is_terminal());
        }
    }
}
