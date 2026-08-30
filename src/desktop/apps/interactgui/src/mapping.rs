use std::fmt;
use std::path::PathBuf;

use cosmix_interaction_schema::{
    DialogActionRoleV1, DialogCommonV1, DialogPresentationV1, DialogProgressCompletionV1,
    DialogProgressValueV1, DialogRequestV1, DialogSeverityV1, DialogValueV1,
};
use ctk::prelude::{
    ActionRole, ChoiceItem, ChoiceSpec, ConfirmSpec, DialogCommon, FileFilter, FileRequest,
    FileRequestId, FileRequestOutcome, InteractionAction, InteractionKind, InteractionOutcome,
    InteractionRequest, InteractionSeverity, InteractionValue, MessageSpec, MultiChoiceSpec,
    ProgressCompletion, ProgressSpec, ProgressValue, PromptSpec, SliderSpec, TextValidator,
    TextViewSpec,
};

const FILE_REQUEST_ID_PLACEHOLDER: FileRequestId = FileRequestId(0);

#[derive(Debug)]
pub enum UiEmission {
    Interaction(InteractionRequest),
    File(FileRequest),
}

#[derive(Debug, PartialEq, Eq)]
pub enum UiOutcome {
    Interaction(InteractionOutcome),
    File(FileRequestOutcome),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MapError(String);

impl MapError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for MapError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for MapError {}

pub fn ingress(presentation: &DialogPresentationV1) -> Result<UiEmission, MapError> {
    presentation
        .dialog
        .validate()
        .map_err(|error| MapError::new(format!("invalid dialog presentation: {error}")))?;

    let emission = match &presentation.dialog {
        DialogRequestV1::Message { common, details } => {
            let common =
                interaction_common(common, message_and_details(common, details.as_deref()));
            UiEmission::Interaction(InteractionRequest::from_kind(
                common,
                InteractionKind::Message(MessageSpec::new()),
            ))
        }
        DialogRequestV1::Confirm { common, actions } => {
            let actions = actions.iter().map(|action| {
                let mut mapped = InteractionAction::new(
                    action.key.clone(),
                    action.label.clone(),
                    action_role(action.role),
                );
                if action.is_default {
                    mapped = mapped.default();
                }
                mapped
            });
            UiEmission::Interaction(InteractionRequest::from_kind(
                interaction_common(common, common.message.clone()),
                InteractionKind::Confirm(ConfirmSpec::new(actions)),
            ))
        }
        DialogRequestV1::Prompt { common, input } => {
            if input.multiline {
                return Err(MapError::new(
                    "multiline prompts are unsupported by CTK PromptSpec",
                ));
            }
            let max_length = usize::try_from(input.max_chars)
                .map_err(|_| MapError::new("prompt max_chars does not fit usize"))?;
            let mut request = InteractionRequest::from_kind(
                interaction_common(common, common.message.clone()),
                InteractionKind::Prompt(PromptSpec::new(input.initial.clone().unwrap_or_default())),
            )
            .max_length(max_length);
            if input.required {
                request = request.validator(TextValidator::new(|value| {
                    if value.is_empty() {
                        Err("A value is required".into())
                    } else {
                        Ok(value.to_owned())
                    }
                }));
            }
            UiEmission::Interaction(request)
        }
        DialogRequestV1::Choice {
            common,
            items,
            initial,
        } => {
            let items = items.iter().map(choice_item);
            let mut request = InteractionRequest::from_kind(
                interaction_common(common, common.message.clone()),
                InteractionKind::Choice(ChoiceSpec::new(items)),
            );
            if let Some(initial) = initial {
                request = request.initial_choice(initial.clone());
            }
            UiEmission::Interaction(request)
        }
        DialogRequestV1::MultiChoice {
            common,
            items,
            initial,
        } => {
            let items = items.iter().map(choice_item);
            let request = InteractionRequest::from_kind(
                interaction_common(common, common.message.clone()),
                InteractionKind::MultiChoice(MultiChoiceSpec::new(items)),
            )
            .initial_choices(initial.iter().cloned());
            UiEmission::Interaction(request)
        }
        DialogRequestV1::Progress {
            common,
            progress,
            cancellable,
        } => {
            let snapshot = presentation.progress.as_ref();
            let progress = snapshot.map_or(progress, |snapshot| &snapshot.progress);
            let message = snapshot
                .and_then(|snapshot| snapshot.message.clone())
                .or_else(|| common.message.clone());
            let mut request = InteractionRequest::from_kind(
                interaction_common(common, message),
                InteractionKind::Progress(ProgressSpec::new(progress_value(progress))),
            );
            if *cancellable && !presentation.cancel_requested {
                request = request.cancellable();
            }
            UiEmission::Interaction(request)
        }
        DialogRequestV1::FileOpen {
            common,
            initial_directory,
            filters,
            multiple,
        } => {
            if *multiple {
                return Err(MapError::new(
                    "multiple file selection is unsupported by CTK FileRequest",
                ));
            }
            let mut request =
                FileRequest::open_file(FILE_REQUEST_ID_PLACEHOLDER, common.title.clone());
            request.initial_directory = initial_directory.as_ref().map(PathBuf::from);
            request.filters = filters
                .iter()
                .map(|filter| FileFilter::new(filter.label.clone(), filter.extensions.clone()))
                .collect();
            UiEmission::File(request)
        }
        DialogRequestV1::FileSave {
            common,
            initial_directory,
            filters,
            suggested_name,
            default_extension,
        } => {
            let mut request =
                FileRequest::save_file(FILE_REQUEST_ID_PLACEHOLDER, common.title.clone());
            request.initial_directory = initial_directory.as_ref().map(PathBuf::from);
            request.filters = filters
                .iter()
                .map(|filter| FileFilter::new(filter.label.clone(), filter.extensions.clone()))
                .collect();
            request.suggested_name.clone_from(suggested_name);
            request.default_extension.clone_from(default_extension);
            UiEmission::File(request)
        }
        DialogRequestV1::DirSelect {
            common,
            initial_directory,
        } => {
            let mut request =
                FileRequest::select_directory(FILE_REQUEST_ID_PLACEHOLDER, common.title.clone());
            request.initial_directory = initial_directory.as_ref().map(PathBuf::from);
            UiEmission::File(request)
        }
        DialogRequestV1::Slider {
            common,
            min,
            max,
            step,
            initial,
        } => UiEmission::Interaction(InteractionRequest::from_kind(
            interaction_common(common, common.message.clone()),
            InteractionKind::Slider(SliderSpec::new(*min, *max, *step, *initial)),
        )),
        DialogRequestV1::TextView {
            common,
            text,
            monospace,
        } => {
            let mut request = InteractionRequest::from_kind(
                interaction_common(common, common.message.clone()),
                InteractionKind::TextView(TextViewSpec::new(text.clone())),
            );
            if *monospace {
                request = request.monospace();
            }
            UiEmission::Interaction(request)
        }
    };
    Ok(emission)
}

pub fn egress(request: &DialogRequestV1, outcome: UiOutcome) -> Result<DialogValueV1, MapError> {
    let value = match outcome {
        UiOutcome::Interaction(outcome) => interaction_egress(request, outcome)?,
        UiOutcome::File(outcome) => file_egress(request, outcome)?,
    };
    value
        .validate_for(request)
        .map_err(|error| MapError::new(format!("mapped result failed validation: {error}")))?;
    Ok(value)
}

fn interaction_egress(
    request: &DialogRequestV1,
    outcome: InteractionOutcome,
) -> Result<DialogValueV1, MapError> {
    match outcome {
        InteractionOutcome::Resolved(value) => resolved_value(request, value),
        InteractionOutcome::Action(action) => action_value(request, action),
        InteractionOutcome::Cancelled => Err(MapError::new(
            "CTK cancellation has no presenter-side dialog.v1 value",
        )),
        InteractionOutcome::Dismissed => Err(MapError::new(
            "CTK dismissal has no presenter-side dialog.v1 value",
        )),
        _ => Err(MapError::new("unsupported future CTK interaction outcome")),
    }
}

fn resolved_value(
    request: &DialogRequestV1,
    value: InteractionValue,
) -> Result<DialogValueV1, MapError> {
    match value {
        InteractionValue::Acknowledged => match request {
            DialogRequestV1::Message { .. } => Ok(DialogValueV1::Message {}),
            DialogRequestV1::TextView { .. } => Ok(DialogValueV1::TextView {}),
            _ => Err(MapError::new("acknowledgement does not match dialog kind")),
        },
        InteractionValue::Action(action) => action_value(request, action),
        InteractionValue::Text(text) => Ok(DialogValueV1::Prompt { text }),
        InteractionValue::Choice(key) => Ok(DialogValueV1::Choice { key }),
        InteractionValue::MultiChoice(keys) => Ok(DialogValueV1::MultiChoice { keys }),
        InteractionValue::Progress(completion) => Ok(DialogValueV1::Progress {
            completion: progress_completion(completion)?,
        }),
        InteractionValue::Slider(value) => Ok(DialogValueV1::Slider { value }),
        InteractionValue::Secret(_) => Err(MapError::new(
            "secret prompt results are forbidden on dialog.v1",
        )),
        _ => Err(MapError::new("unsupported future CTK interaction value")),
    }
}

fn action_value(request: &DialogRequestV1, action: String) -> Result<DialogValueV1, MapError> {
    match request {
        DialogRequestV1::Message { .. } if action == "ok" => Ok(DialogValueV1::Message {}),
        DialogRequestV1::Confirm { .. } => Ok(DialogValueV1::Confirm { action }),
        _ => Err(MapError::new("action result does not match dialog kind")),
    }
}

fn file_egress(
    request: &DialogRequestV1,
    outcome: FileRequestOutcome,
) -> Result<DialogValueV1, MapError> {
    let FileRequestOutcome::Selected(paths) = outcome else {
        return Err(match outcome {
            FileRequestOutcome::Cancelled => {
                MapError::new("CTK file cancellation has no presenter-side dialog.v1 value")
            }
            FileRequestOutcome::Failed(message) => {
                MapError::new(format!("CTK file requester failed: {message}"))
            }
            FileRequestOutcome::Selected(_) => unreachable!(),
        });
    };
    let paths = paths
        .into_iter()
        .map(|path| {
            path.into_os_string()
                .into_string()
                .map_err(|_| MapError::new("selected path is not valid UTF-8"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    match request {
        DialogRequestV1::FileOpen { .. } => Ok(DialogValueV1::FileOpen { paths }),
        DialogRequestV1::FileSave { .. } => match paths.as_slice() {
            [path] => Ok(DialogValueV1::FileSave { path: path.clone() }),
            _ => Err(MapError::new("file-save must return exactly one path")),
        },
        DialogRequestV1::DirSelect { .. } => match paths.as_slice() {
            [path] => Ok(DialogValueV1::DirSelect { path: path.clone() }),
            _ => Err(MapError::new("dir-select must return exactly one path")),
        },
        _ => Err(MapError::new("file result does not match dialog kind")),
    }
}

fn interaction_common(common: &DialogCommonV1, message: Option<String>) -> DialogCommon {
    let mut mapped = DialogCommon::new(common.title.clone()).severity(match common.severity {
        DialogSeverityV1::Info => InteractionSeverity::Info,
        DialogSeverityV1::Success => InteractionSeverity::Success,
        DialogSeverityV1::Warning => InteractionSeverity::Warning,
        DialogSeverityV1::Error => InteractionSeverity::Danger,
    });
    if let Some(message) = message {
        mapped = mapped.message(message);
    }
    mapped
}

fn message_and_details(common: &DialogCommonV1, details: Option<&str>) -> Option<String> {
    match (common.message.as_deref(), details) {
        (Some(message), Some(details)) => Some(format!("{message}\n\n{details}")),
        (Some(message), None) => Some(message.to_owned()),
        (None, Some(details)) => Some(details.to_owned()),
        (None, None) => None,
    }
}

fn action_role(role: DialogActionRoleV1) -> ActionRole {
    match role {
        DialogActionRoleV1::Accept => ActionRole::Accept,
        DialogActionRoleV1::Cancel => ActionRole::Cancel,
        DialogActionRoleV1::Destructive => ActionRole::Destructive,
        DialogActionRoleV1::Auxiliary => ActionRole::Auxiliary,
    }
}

fn choice_item(item: &cosmix_interaction_schema::DialogChoiceItemV1) -> ChoiceItem {
    let mut mapped = ChoiceItem::new(item.key.clone(), item.label.clone());
    if let Some(description) = &item.description {
        mapped = mapped.description(description.clone());
    }
    if !item.enabled {
        mapped = mapped.disabled();
    }
    mapped
}

fn progress_value(value: &DialogProgressValueV1) -> ProgressValue {
    match value {
        DialogProgressValueV1::Indeterminate {} => ProgressValue::Indeterminate,
        DialogProgressValueV1::Determinate { current, total } => ProgressValue::Determinate {
            current: *current,
            total: *total,
        },
    }
}

fn progress_completion(
    completion: ProgressCompletion,
) -> Result<DialogProgressCompletionV1, MapError> {
    match completion {
        ProgressCompletion::Succeeded => Ok(DialogProgressCompletionV1::Succeeded {}),
        ProgressCompletion::Cancelled => Ok(DialogProgressCompletionV1::Cancelled {}),
        ProgressCompletion::Failed(message) => Ok(DialogProgressCompletionV1::Failed { message }),
        _ => Err(MapError::new("unsupported future CTK progress completion")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_interaction_schema::{
        DialogActionV1, DialogChoiceItemV1, DialogFileFilterV1, DialogProgressSnapshotV1,
        DialogTextInputV1,
    };

    fn common() -> DialogCommonV1 {
        DialogCommonV1 {
            title: "Test".into(),
            message: Some("Message".into()),
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

    fn presentation(dialog: DialogRequestV1) -> DialogPresentationV1 {
        DialogPresentationV1 {
            handle: "dialog-1".into(),
            attempt_token: 7,
            dialog,
            progress: None,
            cancel_requested: false,
        }
    }

    fn supported_cases() -> Vec<(DialogRequestV1, UiOutcome, DialogValueV1)> {
        vec![
            (
                DialogRequestV1::Message {
                    common: common(),
                    details: Some("Details".into()),
                },
                UiOutcome::Interaction(InteractionOutcome::Resolved(
                    InteractionValue::Acknowledged,
                )),
                DialogValueV1::Message {},
            ),
            (
                DialogRequestV1::Confirm {
                    common: common(),
                    actions: vec![
                        DialogActionV1 {
                            key: "yes".into(),
                            label: "Yes".into(),
                            role: DialogActionRoleV1::Accept,
                            is_default: true,
                        },
                        DialogActionV1 {
                            key: "no".into(),
                            label: "No".into(),
                            role: DialogActionRoleV1::Cancel,
                            is_default: false,
                        },
                    ],
                },
                UiOutcome::Interaction(InteractionOutcome::Action("yes".into())),
                DialogValueV1::Confirm {
                    action: "yes".into(),
                },
            ),
            (
                DialogRequestV1::Prompt {
                    common: common(),
                    input: DialogTextInputV1 {
                        initial: Some("old".into()),
                        max_chars: 20,
                        required: true,
                        multiline: false,
                    },
                },
                UiOutcome::Interaction(InteractionOutcome::Resolved(InteractionValue::Text(
                    "new".into(),
                ))),
                DialogValueV1::Prompt { text: "new".into() },
            ),
            (
                DialogRequestV1::Choice {
                    common: common(),
                    items: vec![item("one"), item("two")],
                    initial: Some("one".into()),
                },
                UiOutcome::Interaction(InteractionOutcome::Resolved(InteractionValue::Choice(
                    "two".into(),
                ))),
                DialogValueV1::Choice { key: "two".into() },
            ),
            (
                DialogRequestV1::MultiChoice {
                    common: common(),
                    items: vec![item("one"), item("two")],
                    initial: vec!["one".into()],
                },
                UiOutcome::Interaction(InteractionOutcome::Resolved(
                    InteractionValue::MultiChoice(vec!["one".into(), "two".into()]),
                )),
                DialogValueV1::MultiChoice {
                    keys: vec!["one".into(), "two".into()],
                },
            ),
            (
                DialogRequestV1::Progress {
                    common: common(),
                    progress: DialogProgressValueV1::Determinate {
                        current: 2,
                        total: 10,
                    },
                    cancellable: true,
                },
                UiOutcome::Interaction(InteractionOutcome::Resolved(InteractionValue::Progress(
                    ProgressCompletion::Succeeded,
                ))),
                DialogValueV1::Progress {
                    completion: DialogProgressCompletionV1::Succeeded {},
                },
            ),
            (
                DialogRequestV1::FileOpen {
                    common: common(),
                    initial_directory: Some("/tmp".into()),
                    filters: vec![DialogFileFilterV1 {
                        label: "Text".into(),
                        extensions: vec!["txt".into()],
                    }],
                    multiple: false,
                },
                UiOutcome::File(FileRequestOutcome::Selected(vec![PathBuf::from(
                    "/tmp/a.txt",
                )])),
                DialogValueV1::FileOpen {
                    paths: vec!["/tmp/a.txt".into()],
                },
            ),
            (
                DialogRequestV1::FileSave {
                    common: common(),
                    initial_directory: Some("/tmp".into()),
                    filters: vec![],
                    suggested_name: Some("a.txt".into()),
                    default_extension: None,
                },
                UiOutcome::File(FileRequestOutcome::Selected(vec![PathBuf::from(
                    "/tmp/a.txt",
                )])),
                DialogValueV1::FileSave {
                    path: "/tmp/a.txt".into(),
                },
            ),
            (
                DialogRequestV1::DirSelect {
                    common: common(),
                    initial_directory: Some("/tmp".into()),
                },
                UiOutcome::File(FileRequestOutcome::Selected(vec![PathBuf::from("/tmp")])),
                DialogValueV1::DirSelect {
                    path: "/tmp".into(),
                },
            ),
            (
                DialogRequestV1::Slider {
                    common: common(),
                    min: 0,
                    max: 10,
                    step: 2,
                    initial: 4,
                },
                UiOutcome::Interaction(InteractionOutcome::Resolved(InteractionValue::Slider(8))),
                DialogValueV1::Slider { value: 8 },
            ),
            (
                DialogRequestV1::TextView {
                    common: common(),
                    text: "Long text".into(),
                    monospace: true,
                },
                UiOutcome::Interaction(InteractionOutcome::Resolved(
                    InteractionValue::Acknowledged,
                )),
                DialogValueV1::TextView {},
            ),
        ]
    }

    #[test]
    fn every_wire_kind_maps_in_both_directions() {
        for (request, outcome, expected) in supported_cases() {
            ingress(&presentation(request.clone())).expect("ingress mapping");
            assert_eq!(egress(&request, outcome).expect("egress mapping"), expected);
        }
    }

    #[test]
    fn progress_snapshot_overrides_stale_initial_value() {
        let mut presentation = presentation(DialogRequestV1::Progress {
            common: common(),
            progress: DialogProgressValueV1::Indeterminate {},
            cancellable: true,
        });
        presentation.progress = Some(DialogProgressSnapshotV1 {
            message: Some("Fresh".into()),
            progress: DialogProgressValueV1::Determinate {
                current: 75,
                total: 100,
            },
        });
        presentation.cancel_requested = true;
        assert!(matches!(
            ingress(&presentation),
            Ok(UiEmission::Interaction(_))
        ));
    }

    #[test]
    fn unsupported_public_api_shapes_are_errors_not_panics() {
        let multiline = DialogRequestV1::Prompt {
            common: common(),
            input: DialogTextInputV1 {
                initial: None,
                max_chars: 20,
                required: false,
                multiline: true,
            },
        };
        let multiple = DialogRequestV1::FileOpen {
            common: common(),
            initial_directory: None,
            filters: vec![],
            multiple: true,
        };
        for request in [multiline, multiple] {
            let result = std::panic::catch_unwind(|| ingress(&presentation(request)));
            assert!(matches!(result, Ok(Err(_))));
        }
    }

    #[test]
    fn terminal_non_values_and_invalid_actions_fail_mapping() {
        let message = DialogRequestV1::Message {
            common: common(),
            details: None,
        };
        for outcome in [
            InteractionOutcome::Cancelled,
            InteractionOutcome::Dismissed,
            InteractionOutcome::Action("unexpected".into()),
        ] {
            assert!(egress(&message, UiOutcome::Interaction(outcome)).is_err());
        }

        let confirm = supported_cases().remove(1).0;
        assert!(
            egress(
                &confirm,
                UiOutcome::Interaction(InteractionOutcome::Action("unknown".into())),
            )
            .is_err(),
            "DialogValueV1::validate_for must reject an unknown action"
        );
    }

    #[test]
    fn file_cancellation_and_failure_are_mapping_errors() {
        let request = supported_cases().remove(6).0;
        assert!(egress(&request, UiOutcome::File(FileRequestOutcome::Cancelled)).is_err());
        assert!(egress(
            &request,
            UiOutcome::File(FileRequestOutcome::Failed("picker failed".into())),
        )
        .is_err());
    }
}
