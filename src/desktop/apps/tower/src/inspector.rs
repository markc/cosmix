//! Citizen app/action/control projections used by the P2 inspector.

use std::collections::BTreeMap;

use bevy::ecs::message::Message;
use serde::Deserialize;
use serde_json::Value;

pub(crate) const MAX_INSPECTOR_ITEMS: usize = 256;
pub(crate) const MAX_INSPECTOR_ENTITIES: usize = 768;
const MAX_ID_BYTES: usize = 128;
const MAX_SHORT_TEXT_CHARS: usize = 128;
const MAX_LABEL_CHARS: usize = 256;
const MAX_DESCRIPTION_CHARS: usize = 512;
const MAX_SCHEMA_FIELDS: usize = 64;
const MAX_VERBS: usize = 128;
const MAX_CHOICES: usize = 64;
const MAX_VALUE_BYTES: usize = 4 * 1024;
const MAX_INSPECTOR_PAYLOAD_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ProcessIdentity {
    pub service: String,
    pub pid: Option<u32>,
    pub started_at: Option<String>,
}

impl ProcessIdentity {
    pub(crate) fn is_known(&self) -> bool {
        self.pid.is_some() || self.started_at.is_some()
    }

    pub(crate) fn same_process(&self, current: &Self) -> bool {
        self.is_known() && current.is_known() && self == current
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum MutationTarget {
    Action { service: String, action: String },
    Control { service: String, control: String },
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub(crate) struct AppDescription {
    pub app: String,
    pub title: String,
    pub view: String,
    pub engine: String,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub controls: usize,
    #[serde(default)]
    pub verbs: Vec<String>,
}

impl AppDescription {
    pub(crate) fn parse(body: &str) -> Result<Self, String> {
        reject_oversize_payload(body, "app.describe")?;
        let mut description: Self =
            serde_json::from_str(body).map_err(|error| format!("invalid app.describe: {error}"))?;
        if description.pid.is_none() {
            description.pid = description
                .app
                .rsplit_once('-')
                .and_then(|(_, suffix)| suffix.parse().ok());
        }
        if !valid_id(&description.app) {
            return Err("invalid app.describe app identity".into());
        }
        truncate_text(&mut description.title, MAX_LABEL_CHARS);
        truncate_text(&mut description.view, MAX_SHORT_TEXT_CHARS);
        truncate_text(&mut description.engine, MAX_SHORT_TEXT_CHARS);
        description.verbs.retain(|verb| valid_id(verb));
        description.verbs.sort();
        description.verbs.dedup();
        description.verbs.truncate(MAX_VERBS);
        Ok(description)
    }

    pub(crate) fn has_verb(&self, verb: &str) -> bool {
        self.verbs
            .binary_search_by(|candidate| candidate.as_str().cmp(verb))
            .is_ok()
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub(crate) struct ActionArgument {
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub required: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub(crate) struct ActionArgsSchema {
    #[serde(default)]
    pub fields: Vec<ActionArgument>,
    #[serde(default)]
    pub allow_extra: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub(crate) struct ActionSources {
    #[serde(default)]
    pub bus: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub(crate) struct ActionDescriptor {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub args_schema: ActionArgsSchema,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub icon_name: Option<String>,
    #[serde(default)]
    pub shortcut: Option<Value>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub interactive: Option<Value>,
    #[serde(default)]
    pub allowed_sources: ActionSources,
    #[serde(default)]
    pub enabled: bool,
}

impl ActionDescriptor {
    pub(crate) fn can_invoke_without_args(&self) -> bool {
        self.enabled
            && self.allowed_sources.bus
            && self.interactive.is_none()
            && !self.args_schema.fields.iter().any(|field| field.required)
    }
}

#[derive(Deserialize)]
struct ActionsList {
    actions: Vec<ActionDescriptor>,
}

#[derive(Deserialize)]
struct ActionDescription {
    action: ActionDescriptor,
}

#[derive(Debug)]
pub(crate) struct ParsedActions {
    pub actions: Vec<ActionDescriptor>,
    pub omitted: usize,
}

pub(crate) fn parse_actions_list(body: &str) -> Result<ParsedActions, String> {
    reject_oversize_payload(body, "actions.list")?;
    let actions: ActionsList =
        serde_json::from_str(body).map_err(|error| format!("invalid actions.list: {error}"))?;
    let total = actions.actions.len();
    let mut actions: Vec<_> = actions
        .actions
        .into_iter()
        .filter_map(sanitise_action)
        .collect();
    actions.sort_by(|left, right| {
        left.category
            .cmp(&right.category)
            .then_with(|| left.label.cmp(&right.label))
            .then_with(|| left.id.cmp(&right.id))
    });
    actions.truncate(MAX_INSPECTOR_ITEMS);
    Ok(ParsedActions {
        omitted: total.saturating_sub(actions.len()),
        actions,
    })
}

pub(crate) fn parse_action_description(body: &str) -> Result<ActionDescriptor, String> {
    reject_oversize_payload(body, "actions.describe")?;
    let description = serde_json::from_str::<ActionDescription>(body)
        .map_err(|error| format!("invalid actions.describe: {error}"))?;
    sanitise_action(description.action)
        .ok_or_else(|| "invalid actions.describe action identity".into())
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub(crate) struct ControlDescriptor {
    pub id: String,
    pub kind: String,
    pub value_type: String,
    #[serde(default)]
    pub queryable: bool,
    #[serde(default)]
    pub writable: bool,
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    #[serde(default)]
    pub step: Option<f64>,
    #[serde(default)]
    pub unit: Option<String>,
    #[serde(default)]
    pub choices: Vec<Value>,
    #[serde(default)]
    pub action: Option<String>,
    #[serde(skip)]
    pub value: Option<Value>,
    #[serde(skip)]
    pub value_observed_at_ms: Option<u64>,
    #[serde(skip)]
    pub value_error: Option<String>,
}

#[derive(Deserialize)]
struct ControlsList {
    controls: Vec<ControlDescriptor>,
}

#[derive(Deserialize)]
struct ControlValue {
    id: String,
    value: Value,
}

#[derive(Debug)]
pub(crate) struct ParsedControls {
    pub controls: Vec<ControlDescriptor>,
    pub omitted: usize,
}

pub(crate) fn parse_controls_list(body: &str) -> Result<ParsedControls, String> {
    reject_oversize_payload(body, "app.controls.list")?;
    let controls: ControlsList = serde_json::from_str(body)
        .map_err(|error| format!("invalid app.controls.list: {error}"))?;
    let total = controls.controls.len();
    let mut controls: Vec<_> = controls
        .controls
        .into_iter()
        .filter_map(sanitise_control)
        .collect();
    controls.sort_by(|left, right| left.id.cmp(&right.id));
    controls.truncate(MAX_INSPECTOR_ITEMS);
    Ok(ParsedControls {
        omitted: total.saturating_sub(controls.len()),
        controls,
    })
}

pub(crate) fn parse_control_value(body: &str) -> Result<(String, Value), String> {
    if body.len() > MAX_VALUE_BYTES {
        return Err("app.controls.get value exceeds 4096 bytes".into());
    }
    let value = serde_json::from_str::<ControlValue>(body)
        .map_err(|error| format!("invalid app.controls.get: {error}"))?;
    if !valid_id(&value.id) {
        return Err("invalid app.controls.get control identity".into());
    }
    Ok((value.id, value.value))
}

fn sanitise_action(mut action: ActionDescriptor) -> Option<ActionDescriptor> {
    if !valid_id(&action.id) {
        return None;
    }
    truncate_text(&mut action.label, MAX_LABEL_CHARS);
    truncate_optional(&mut action.category, MAX_SHORT_TEXT_CHARS);
    truncate_optional(&mut action.icon_name, MAX_SHORT_TEXT_CHARS);
    truncate_optional(&mut action.description, MAX_DESCRIPTION_CHARS);
    action.args_schema.fields.truncate(MAX_SCHEMA_FIELDS);
    for field in &mut action.args_schema.fields {
        truncate_text(&mut field.name, MAX_SHORT_TEXT_CHARS);
        truncate_text(&mut field.kind, MAX_SHORT_TEXT_CHARS);
    }
    if action
        .shortcut
        .as_ref()
        .is_some_and(|shortcut| shortcut.to_string().len() > MAX_VALUE_BYTES)
    {
        action.shortcut = Some(Value::String("metadata omitted: oversize".into()));
    }
    if action.interactive.is_some() {
        action.interactive = Some(Value::Bool(true));
    }
    Some(action)
}

fn sanitise_control(mut control: ControlDescriptor) -> Option<ControlDescriptor> {
    if !valid_id(&control.id) {
        return None;
    }
    truncate_text(&mut control.kind, MAX_SHORT_TEXT_CHARS);
    truncate_text(&mut control.value_type, MAX_SHORT_TEXT_CHARS);
    truncate_optional(&mut control.unit, MAX_SHORT_TEXT_CHARS);
    if control
        .action
        .as_ref()
        .is_some_and(|action| !valid_id(action))
    {
        control.action = None;
    }
    control.choices.truncate(MAX_CHOICES);
    for choice in &mut control.choices {
        if choice.to_string().len() > MAX_VALUE_BYTES {
            *choice = Value::String("metadata omitted: oversize".into());
        }
    }
    Some(control)
}

fn reject_oversize_payload(body: &str, verb: &str) -> Result<(), String> {
    if body.len() > MAX_INSPECTOR_PAYLOAD_BYTES {
        return Err(format!("{verb} payload exceeds 1048576 bytes"));
    }
    Ok(())
}

fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_ID_BYTES
}

fn truncate_optional(value: &mut Option<String>, max_chars: usize) {
    if let Some(value) = value {
        truncate_text(value, max_chars);
    }
}

pub(crate) fn truncate_text(value: &mut String, max_chars: usize) {
    let Some((boundary, _)) = value.char_indices().nth(max_chars) else {
        return;
    };
    value.truncate(boundary);
    value.push_str("...");
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct InspectorResult {
    pub summary: String,
    pub ok: bool,
    pub body: Option<Value>,
    pub observed_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CitizenInspector {
    pub service: String,
    pub identity: ProcessIdentity,
    pub description: Option<AppDescription>,
    pub description_error: Option<String>,
    pub description_observed_at_ms: Option<u64>,
    pub actions: BTreeMap<String, ActionDescriptor>,
    pub actions_error: Option<String>,
    pub actions_observed_at_ms: Option<u64>,
    pub actions_omitted: usize,
    pub controls: BTreeMap<String, ControlDescriptor>,
    pub controls_error: Option<String>,
    pub controls_observed_at_ms: Option<u64>,
    pub controls_omitted: usize,
    pub result: Option<InspectorResult>,
}

impl CitizenInspector {
    pub(crate) fn pending(identity: ProcessIdentity) -> Self {
        Self {
            service: identity.service.clone(),
            identity,
            description: None,
            description_error: None,
            description_observed_at_ms: None,
            actions: BTreeMap::new(),
            actions_error: None,
            actions_observed_at_ms: None,
            actions_omitted: 0,
            controls: BTreeMap::new(),
            controls_error: None,
            controls_observed_at_ms: None,
            controls_omitted: 0,
            result: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum InspectorMutation {
    InvokeAction {
        service: String,
        action: String,
        identity: ProcessIdentity,
    },
    SetControl {
        service: String,
        control: String,
        value: Value,
        identity: ProcessIdentity,
    },
}

#[derive(Message, Clone, Debug)]
pub(crate) struct InspectCitizen {
    pub service: String,
}

#[derive(Message, Clone, Debug)]
pub(crate) struct InspectorMutationRequest(pub InspectorMutation);

impl InspectorMutation {
    pub(crate) fn identity(&self) -> &ProcessIdentity {
        match self {
            Self::InvokeAction { identity, .. } | Self::SetControl { identity, .. } => identity,
        }
    }

    pub(crate) fn target(&self) -> MutationTarget {
        match self {
            Self::InvokeAction {
                service, action, ..
            } => MutationTarget::Action {
                service: service.clone(),
                action: action.clone(),
            },
            Self::SetControl {
                service, control, ..
            } => MutationTarget::Control {
                service: service.clone(),
                control: control.clone(),
            },
        }
    }

    pub(crate) fn confirmation(&self) -> (String, String) {
        match self {
            Self::InvokeAction {
                service, action, ..
            } => (
                format!("Invoke {action}?"),
                format!("Send action.invoke to the local process {service}."),
            ),
            Self::SetControl {
                service,
                control,
                value,
                ..
            } => (
                format!("Set {control}?"),
                format!("Send app.controls.set value {value} to the local process {service}."),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_app_action_and_control_fixtures() {
        let app = AppDescription::parse(include_str!("fixtures/app_describe.json")).unwrap();
        assert_eq!(app.view, "studio");
        assert_eq!(app.engine, "bevy");
        assert_eq!(app.pid, Some(4242));
        assert!(app.has_verb("app.controls.list"));

        let actions = parse_actions_list(include_str!("fixtures/actions_list.json")).unwrap();
        assert_eq!(actions.actions.len(), 2);
        assert_eq!(actions.actions[0].category.as_deref(), Some("file"));
        assert_eq!(actions.actions[0].icon_name.as_deref(), Some("folder-open"));
        assert_eq!(
            actions.actions[0].shortcut,
            Some(serde_json::json!("Ctrl+O"))
        );
        assert!(!actions.actions[0].can_invoke_without_args());
        assert!(actions.actions[1].can_invoke_without_args());

        let described =
            parse_action_description(include_str!("fixtures/actions_describe.json")).unwrap();
        assert_eq!(described.id, "transport.toggle");

        let controls = parse_controls_list(include_str!("fixtures/controls_list.json")).unwrap();
        assert_eq!(controls.controls.len(), 3);
        assert_eq!(controls.controls[0].id, "mute");
        assert!(controls.controls[0].writable);
        let trim = controls
            .controls
            .iter()
            .find(|control| control.id == "trim")
            .unwrap();
        assert_eq!(trim.step, Some(0.5));
        let action = controls
            .controls
            .iter()
            .find(|control| control.id == "transport.toggle")
            .unwrap();
        assert_eq!(action.value_type, "action");
        assert_eq!(action.action.as_deref(), Some("transport.toggle"));
        assert_eq!(
            parse_control_value(include_str!("fixtures/control_get.json")).unwrap(),
            ("trim".into(), serde_json::json!(-6.0))
        );
    }

    #[test]
    fn hostile_lists_and_multibyte_text_are_bounded() {
        let actions = (0..300)
            .map(|index| {
                serde_json::json!({
                    "id": format!("action-{index}"),
                    "label": "界".repeat(400),
                    "description": "x".repeat(2_000),
                    "allowed_sources": {"bus": true},
                    "enabled": true
                })
            })
            .collect::<Vec<_>>();
        let parsed = parse_actions_list(&serde_json::json!({"actions": actions}).to_string())
            .expect("bounded hostile action list");
        assert_eq!(parsed.actions.len(), MAX_INSPECTOR_ITEMS);
        assert_eq!(parsed.omitted, 300 - MAX_INSPECTOR_ITEMS);
        assert!(parsed.actions[0].label.ends_with("..."));
        assert!(parsed.actions[0]
            .label
            .is_char_boundary(parsed.actions[0].label.len()));
        assert!(parsed.actions[0]
            .description
            .as_ref()
            .is_some_and(|description| description.ends_with("...")));

        let mut text = "界".repeat(200);
        truncate_text(&mut text, 17);
        assert_eq!(text.chars().count(), 20);
        assert!(text.ends_with("..."));

        let oversize = "x".repeat(MAX_INSPECTOR_PAYLOAD_BYTES + 1);
        assert_eq!(
            parse_controls_list(&oversize).unwrap_err(),
            "app.controls.list payload exceeds 1048576 bytes"
        );
    }
}
