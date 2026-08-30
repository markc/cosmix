//! Reusable Bus ingress for the CTK action registry.
//!
//! [`ActionPortPlugin`] adds `action.invoke`, `actions.list`, and
//! `actions.describe` to an existing [`crate::app_control::AppPortPlugin`]. The
//! verbs are addressed to the app's exact process-scoped service name; there is
//! no global app alias or implicit instance selection.
//!
//! Authority follows noded's current trust boundary. A registered local
//! connection gets a canonical, broker-rewritten `from` identity and is inside
//! the local trust domain. Mesh ingress has `from` stripped and is rejected.
//! Wire-supplied `source_peer`, `permissions`, or `signed_ident` claims are also
//! rejected. Remote invocation remains closed until noded supplies
//! authenticated provenance suitable for resolving the
//! [`cosmix_mesh_trust::ctk_caps::CTK_ACTIONS`] grant.
//!
//! Apps with same-frame action producers schedule [`crate::app_control::AppPortSystems`]
//! after those producers and before their action routers. The invoke handler
//! then observes enabled interactive requests already produced in that frame,
//! rejects Bus ingress with `modal_active`, and still publishes accepted Bus
//! requests in time for same-frame routing.

use bevy::app::{App, Plugin, Update};
use bevy::ecs::message::{MessageReader, MessageWriter};
use bevy::ecs::resource::Resource;
use bevy::ecs::schedule::IntoScheduleConfigs;
use bevy::ecs::system::{In, Res, ResMut};
use cosmix_actions::{ActionArgs, ActionMeta, ActionRegistry, ActionSource, RegistryError};
use cosmix_mesh_trust::ctk_caps::CTK_ACTIONS;
use serde_json::{json, Map, Value};

use crate::bus::InboundRequest;
use crate::app_control::{
    app_verb_registered, AppPortAppExt, AppPortPlugin, AppPortReply, AppPortRequest,
    LocalCallerError,
};
use crate::menu::{ActionRegistryResource, ActionRequest, Source};
use crate::modal_capture::ModalCapture;

/// Generic registered-action invocation verb.
pub const ACTION_INVOKE_VERB: &str = "action.invoke";
/// Stable action metadata listing verb.
pub const ACTIONS_LIST_VERB: &str = "actions.list";
/// Stable single-action metadata verb.
pub const ACTIONS_DESCRIBE_VERB: &str = "actions.describe";

/// Unknown or unregistered action id.
pub const ACTION_ERROR_UNKNOWN: &str = "action_unknown";
/// Missing, malformed, or schema-invalid invocation arguments.
pub const ACTION_ERROR_INVALID_ARGS: &str = "action_invalid_args";
/// Live enabled predicate rejected invocation.
pub const ACTION_ERROR_DISABLED: &str = "action_disabled";
/// Caller is not a canonically registered local noded service.
pub const ACTION_ERROR_UNREGISTERED_CALLER: &str = "unregistered_caller";
/// Wire-asserted remote identity cannot establish authority.
pub const ACTION_ERROR_REMOTE_IDENTITY_UNAVAILABLE: &str = "remote_identity_unavailable";
/// Bus cannot invoke an action which requires local UI.
pub const ACTION_ERROR_INTERACTIVE: &str = "interactive_action_requires_direct_verb";
/// Bus cannot invoke a local-only interactive action.
pub const ACTION_ERROR_LOCAL_ONLY_INTERACTIVE: &str = "local_only_interactive_action";
/// The action's source allowlist denies Bus.
pub const ACTION_ERROR_SOURCE_NOT_ALLOWED: &str = "action_source_not_allowed";
/// A modal owner or same-frame interactive request currently captures input.
pub const ACTION_ERROR_MODAL_ACTIVE: &str = "modal_active";

/// Stable Bus action-ingress failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionPortError {
    /// Machine-stable identifier.
    pub id: &'static str,
    /// Human-readable diagnostic.
    pub message: String,
    /// Typed alternative for an action that would otherwise open local UI.
    pub direct_verb: Option<String>,
}

impl ActionPortError {
    fn new(id: &'static str, message: impl Into<String>) -> Self {
        Self {
            id,
            message: message.into(),
            direct_verb: None,
        }
    }

    fn interactive(action: &str, direct_verb: Option<&str>) -> Self {
        match direct_verb {
            Some(direct_verb) => Self {
                id: ACTION_ERROR_INTERACTIVE,
                message: format!(
                    "action {action} requires local interaction; use {direct_verb} with an explicit target"
                ),
                direct_verb: Some(direct_verb.to_owned()),
            },
            None => Self {
                id: ACTION_ERROR_LOCAL_ONLY_INTERACTIVE,
                message: format!("action {action} is a local-only interactive action"),
                direct_verb: None,
            },
        }
    }

    fn reply(&self) -> AppPortReply {
        let mut error = json!({
            "id": self.id,
            "message": self.message,
        });
        if let Some(direct_verb) = &self.direct_verb {
            error
                .as_object_mut()
                .expect("error is an object")
                .insert("direct_verb".into(), json!(direct_verb));
        }
        (10, json!({ "error": error }).to_string())
    }
}

/// Install the query and invocation verbs against [`ActionRegistryResource`].
///
/// [`AppPortPlugin`] must be installed first. A missing registry is initialised
/// empty and therefore fails closed until the app replaces it. The plugin
/// publishes accepted invocations onto CTK's ordinary
/// [`ActionRequest`] bus with [`Source::Bus`].
pub struct ActionPortPlugin;

#[derive(Resource, Default)]
struct ActionIngressState {
    interactive_pending: bool,
}

#[derive(Resource, Default)]
struct ActionListCache {
    key: Option<(u64, u64)>,
    body: String,
}

impl Plugin for ActionPortPlugin {
    fn build(&self, app: &mut App) {
        assert!(
            app.is_plugin_added::<AppPortPlugin>(),
            "install AppPortPlugin before ActionPortPlugin"
        );
        app.init_resource::<ActionRegistryResource>()
            .init_resource::<ActionIngressState>()
            .init_resource::<ActionListCache>()
            .add_message::<ActionRequest>()
            .add_systems(
                Update,
                scan_pending_action_requests
                    .in_set(crate::app_control::AppPortSystems)
                    .before(crate::app_control::route_app_port),
            )
            .register_app_verb(ACTION_INVOKE_VERB, invoke_action)
            .register_app_verb(ACTIONS_LIST_VERB, list_actions)
            .register_app_verb(ACTIONS_DESCRIBE_VERB, describe_action);
    }
}

/// Validate one Bus request and construct its ordinary action-bus message.
///
/// Caller authority is checked before registry lookup so unauthorised callers
/// cannot probe action existence. Caller-supplied ids are looked up as strings
/// and are never interned unless they already exist in the registry.
pub fn prepare_action_invocation(
    request: &InboundRequest,
    registry: &ActionRegistry,
    modal_active: bool,
) -> Result<ActionRequest, ActionPortError> {
    authorize_local_caller(request)?;
    if modal_active {
        return Err(ActionPortError::new(
            ACTION_ERROR_MODAL_ACTIVE,
            "action invocation is unavailable while local modal input is active",
        ));
    }
    let payload = request_payload(request)?;
    let id = required_string(&payload, "id")?;
    let meta = registry.metadata_named(id).ok_or_else(|| {
        ActionPortError::new(ACTION_ERROR_UNKNOWN, format!("unknown action {id}"))
    })?;
    let args = invocation_args(&payload)?;
    if let Err(error) = registry.validate_invocation_from(meta.id, &args, ActionSource::Bus) {
        if matches!(error, RegistryError::SourceNotAllowed { .. }) {
            if let Some(interactive) = &meta.interactive {
                return Err(ActionPortError::interactive(
                    meta.id.as_str(),
                    interactive.direct_verb.as_deref(),
                ));
            }
        }
        return Err(registry_error(error));
    }
    Ok(ActionRequest {
        action: meta.id,
        source: Source::Bus,
        args,
        invocation_focus: None,
    })
}

fn invoke_action(
    In(input): In<AppPortRequest>,
    registry: Res<ActionRegistryResource>,
    capture: Option<Res<ModalCapture>>,
    ingress: Res<ActionIngressState>,
    mut messages: MessageWriter<ActionRequest>,
) -> AppPortReply {
    let modal_active =
        ingress.interactive_pending || capture.as_deref().is_some_and(ModalCapture::is_captured);
    match prepare_action_invocation(&input.request, registry.registry(), modal_active) {
        Ok(request) => {
            let action = request.action;
            messages.write(request);
            (0, json!({ "accepted": true, "action": action }).to_string())
        }
        Err(error) => error.reply(),
    }
}

fn list_actions(
    In(input): In<AppPortRequest>,
    registry: Res<ActionRegistryResource>,
    mut cache: ResMut<ActionListCache>,
) -> AppPortReply {
    if let Err(error) = authorize_local_caller(&input.request) {
        return error.reply();
    }
    let key = (registry.registry().revision(), registry.enabled_revision());
    if cache.key == Some(key) {
        return (0, cache.body.clone());
    }
    let actions: Vec<_> = registry
        .registry()
        .iter_metadata()
        .map(|meta| action_description(meta, registry.registry()))
        .collect();
    cache.key = Some(key);
    cache.body = json!({ "actions": actions }).to_string();
    (0, cache.body.clone())
}

fn describe_action(
    In(input): In<AppPortRequest>,
    registry: Res<ActionRegistryResource>,
) -> AppPortReply {
    if let Err(error) = authorize_local_caller(&input.request) {
        return error.reply();
    }
    let payload = match request_payload(&input.request) {
        Ok(payload) => payload,
        Err(error) => return error.reply(),
    };
    let id = match required_string(&payload, "id") {
        Ok(id) => id,
        Err(error) => return error.reply(),
    };
    let Some(meta) = registry.registry().metadata_named(id) else {
        return ActionPortError::new(ACTION_ERROR_UNKNOWN, format!("unknown action {id}")).reply();
    };
    (
        0,
        json!({ "action": action_description(meta, registry.registry()) }).to_string(),
    )
}

fn scan_pending_action_requests(
    mut messages: MessageReader<ActionRequest>,
    registry: Res<ActionRegistryResource>,
    mut ingress: ResMut<ActionIngressState>,
) {
    let mut interactive_pending = false;
    for request in messages.read() {
        interactive_pending |= pending_interactive(request, registry.registry());
    }
    ingress.interactive_pending = interactive_pending;
}

fn pending_interactive(request: &ActionRequest, registry: &ActionRegistry) -> bool {
    request.source != Source::Bus
        && registry
            .metadata(request.action)
            .is_some_and(|meta| meta.interactive.is_some())
        && registry.is_enabled(request.action) == Some(true)
}

/// Validate that every interactive action's advertised direct verb is live in
/// the app-port registry. Local-only interactive actions require no verb.
pub fn validate_action_direct_verbs(app: &App) -> Result<(), Vec<String>> {
    let Some(registry) = app.world().get_resource::<ActionRegistryResource>() else {
        return Ok(());
    };
    let missing: Vec<_> = registry
        .registry()
        .iter_metadata()
        .filter_map(|meta| meta.interactive.as_ref()?.direct_verb.as_ref())
        .filter(|verb| !app_verb_registered(app, verb))
        .cloned()
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(missing)
    }
}

fn action_description(meta: &ActionMeta, registry: &ActionRegistry) -> Value {
    let mut value = serde_json::to_value(meta).expect("ActionMeta serialises");
    value
        .as_object_mut()
        .expect("ActionMeta serialises as an object")
        .insert(
            "enabled".into(),
            json!(registry.is_enabled(meta.id).unwrap_or(false)),
        );
    value
}

fn authorize_local_caller(request: &InboundRequest) -> Result<(), ActionPortError> {
    crate::app_control::authorize_local_caller(request).map_err(|error| match error {
        LocalCallerError::RemoteIdentityUnavailable => ActionPortError::new(
            ACTION_ERROR_REMOTE_IDENTITY_UNAVAILABLE,
            format!(
                "remote ingress is closed until authenticated provenance can resolve {CTK_ACTIONS}"
            ),
        ),
        LocalCallerError::UnregisteredCaller => ActionPortError::new(
            ACTION_ERROR_UNREGISTERED_CALLER,
            "action invocation requires a canonically registered local noded service",
        ),
    })
}

fn request_payload(request: &InboundRequest) -> Result<Map<String, Value>, ActionPortError> {
    let raw = request
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("args"))
        .map(|(_, value)| value.as_str())
        .unwrap_or(&request.body);
    if raw.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str(raw) {
        Ok(Value::Object(payload)) => Ok(payload),
        _ => Err(ActionPortError::new(
            ACTION_ERROR_INVALID_ARGS,
            "request arguments must be a JSON object",
        )),
    }
}

fn required_string<'a>(
    payload: &'a Map<String, Value>,
    name: &str,
) -> Result<&'a str, ActionPortError> {
    payload.get(name).and_then(Value::as_str).ok_or_else(|| {
        ActionPortError::new(
            ACTION_ERROR_INVALID_ARGS,
            format!("argument {name} must be a string"),
        )
    })
}

fn invocation_args(payload: &Map<String, Value>) -> Result<ActionArgs, ActionPortError> {
    match payload.get("args") {
        None => Ok(ActionArgs::new()),
        Some(Value::Object(args)) => serde_json::from_value(Value::Object(args.clone()))
            .map_err(|error| ActionPortError::new(ACTION_ERROR_INVALID_ARGS, error.to_string())),
        Some(_) => Err(ActionPortError::new(
            ACTION_ERROR_INVALID_ARGS,
            "argument args must be an object",
        )),
    }
}

fn registry_error(error: RegistryError) -> ActionPortError {
    match error {
        RegistryError::Disabled(_) => {
            ActionPortError::new(ACTION_ERROR_DISABLED, error.to_string())
        }
        RegistryError::MissingArgument { .. }
        | RegistryError::WrongArgumentType { .. }
        | RegistryError::UnexpectedArgument { .. } => {
            ActionPortError::new(ACTION_ERROR_INVALID_ARGS, error.to_string())
        }
        RegistryError::Unknown(_) => ActionPortError::new(ACTION_ERROR_UNKNOWN, error.to_string()),
        RegistryError::SourceNotAllowed { .. } => {
            ActionPortError::new(ACTION_ERROR_SOURCE_NOT_ALLOWED, error.to_string())
        }
        RegistryError::Duplicate(_)
        | RegistryError::InvalidSchema { .. }
        | RegistryError::InvalidInteractiveDirectVerb { .. }
        | RegistryError::InteractiveBusAllowed { .. }
        | RegistryError::TooManyArguments { .. }
        | RegistryError::RegistryItemLimit { .. }
        | RegistryError::RegistryMetadataLimit { .. }
        | RegistryError::Handler { .. } => {
            ActionPortError::new("action_registry_error", "action registry is invalid")
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use bevy::prelude::App;
    use cosmix_actions::{
        ActionArg, ActionArgKind, ActionId, ActionMeta, ActionRegistry, ActionSources, ArgsSchema,
        InteractiveAction,
    };

    use super::*;
    use crate::app_control::{dispatch_app_request, AppPortPlugin};

    fn registry() -> ActionRegistry {
        let mut registry = ActionRegistry::new();
        for (id, enabled, schema, interactive, allowed_sources) in [
            (
                "transport.toggle",
                true,
                ArgsSchema::default(),
                None,
                ActionSources::BUS,
            ),
            (
                "mixer.gain",
                true,
                ArgsSchema {
                    fields: vec![ActionArg {
                        name: "value".into(),
                        kind: ActionArgKind::Number,
                        required: true,
                        description: None,
                    }],
                    allow_extra: false,
                },
                None,
                ActionSources::BUS,
            ),
            (
                "transport.disabled",
                false,
                ArgsSchema::default(),
                None,
                ActionSources::BUS,
            ),
            (
                "song-open",
                true,
                ArgsSchema::default(),
                Some(InteractiveAction {
                    direct_verb: Some("app.song.load".into()),
                }),
                ActionSources::default(),
            ),
            (
                "settings",
                true,
                ArgsSchema::default(),
                Some(InteractiveAction { direct_verb: None }),
                ActionSources::default(),
            ),
            (
                "settings.disabled",
                false,
                ArgsSchema::default(),
                Some(InteractiveAction { direct_verb: None }),
                ActionSources::default(),
            ),
        ] {
            registry
                .register(
                    ActionMeta {
                        id: ActionId::from_static(id),
                        label: id.into(),
                        args_schema: schema,
                        category: None,
                        icon_name: None,
                        description: None,
                        interactive,
                        allowed_sources,
                    },
                    Arc::new(|_| Ok(())),
                    Arc::new(move || enabled),
                )
                .unwrap();
        }
        registry
    }

    fn request(command: &str, body: Value) -> InboundRequest {
        InboundRequest {
            connection_generation: 1,
            from: "tester".into(),
            command: command.into(),
            headers: BTreeMap::from([("broker_origin".into(), "local".into())]),
            body: body.to_string(),
            reply_id: Some("42".into()),
        }
    }

    fn app() -> App {
        let mut app = App::new();
        let (bridge, _peer) = crate::bus::test_bridge("test-app");
        app.insert_resource(bridge)
            .insert_resource(ActionRegistryResource::new(registry()))
            .add_plugins(AppPortPlugin::new("Test", "test"))
            .add_plugins(ActionPortPlugin);
        app
    }

    fn dispatch(app: &mut App, request: InboundRequest) -> (u8, Value) {
        let (rc, body) = dispatch_app_request(app.world_mut(), "test-app", request);
        (rc, serde_json::from_str(&body).unwrap())
    }

    #[test]
    fn invoke_rejects_unknown_invalid_disabled_unauthorised_and_interactive() {
        let mut app = app();

        let (rc, body) = dispatch(
            &mut app,
            request(ACTION_INVOKE_VERB, json!({ "id": "missing" })),
        );
        assert_eq!(rc, 10);
        assert_eq!(body["error"]["id"], "action_unknown");

        let (rc, body) = dispatch(
            &mut app,
            request(
                ACTION_INVOKE_VERB,
                json!({ "id": "mixer.gain", "args": { "value": "loud" } }),
            ),
        );
        assert_eq!(rc, 10);
        assert_eq!(body["error"]["id"], "action_invalid_args");

        let (rc, body) = dispatch(
            &mut app,
            request(ACTION_INVOKE_VERB, json!({ "id": "transport.disabled" })),
        );
        assert_eq!(rc, 10);
        assert_eq!(body["error"]["id"], "action_disabled");

        let mut unauthorised = request(ACTION_INVOKE_VERB, json!({ "id": "transport.toggle" }));
        unauthorised.from.clear();
        let (rc, body) = dispatch(&mut app, unauthorised);
        assert_eq!(rc, 10);
        assert_eq!(body["error"]["id"], "unregistered_caller");

        for (header, value) in [
            ("source_peer", "remote.example"),
            ("permissions", r#"["ctk.actions"]"#),
            ("Signed_Ident", "mesh:remote.example"),
        ] {
            let mut asserted = request(ACTION_INVOKE_VERB, json!({ "id": "transport.toggle" }));
            asserted.headers.insert(header.into(), value.into());
            let (rc, body) = dispatch(&mut app, asserted);
            assert_eq!(rc, 10);
            assert_eq!(body["error"]["id"], "remote_identity_unavailable");
        }

        let (rc, body) = dispatch(
            &mut app,
            request(ACTION_INVOKE_VERB, json!({ "id": "song-open" })),
        );
        assert_eq!(rc, 10);
        assert_eq!(
            body["error"]["id"],
            "interactive_action_requires_direct_verb"
        );
        assert_eq!(body["error"]["direct_verb"], "app.song.load");

        let (rc, body) = dispatch(
            &mut app,
            request(ACTION_INVOKE_VERB, json!({ "id": "settings" })),
        );
        assert_eq!(rc, 10);
        assert_eq!(body["error"]["id"], "local_only_interactive_action");
        assert!(body["error"].get("direct_verb").is_none());
    }

    #[test]
    fn invoke_publishes_bus_request_and_queries_are_discoverable() {
        let mut app = app();
        let (rc, body) = dispatch(
            &mut app,
            request(ACTION_INVOKE_VERB, json!({ "id": "transport.toggle" })),
        );
        assert_eq!(rc, 0);
        assert_eq!(body["action"], "transport.toggle");
        let messages: Vec<_> = app
            .world_mut()
            .resource_mut::<bevy::ecs::message::Messages<ActionRequest>>()
            .drain()
            .collect();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].action.as_str(), "transport.toggle");
        assert_eq!(messages[0].source, Source::Bus);
        assert!(messages[0].args.is_empty());

        let (rc, _) = dispatch(
            &mut app,
            request(
                ACTION_INVOKE_VERB,
                json!({ "id": "mixer.gain", "args": { "value": -6.5 } }),
            ),
        );
        assert_eq!(rc, 0);
        let typed: Vec<_> = app
            .world_mut()
            .resource_mut::<bevy::ecs::message::Messages<ActionRequest>>()
            .drain()
            .collect();
        assert_eq!(
            typed[0].args.get("value"),
            Some(&cosmix_actions::ActionValue::Number(-6.5))
        );

        let (rc, list) = dispatch(&mut app, request(ACTIONS_LIST_VERB, json!({})));
        assert_eq!(rc, 0);
        assert_eq!(list["actions"].as_array().unwrap().len(), 6);
        let (rc, description) = dispatch(
            &mut app,
            request(ACTIONS_DESCRIBE_VERB, json!({ "id": "song-open" })),
        );
        assert_eq!(rc, 0);
        assert_eq!(
            description["action"]["interactive"]["direct_verb"],
            "app.song.load"
        );

        let (rc, describe) = dispatch(&mut app, request("app.describe", json!({})));
        assert_eq!(rc, 0);
        let verbs = describe["verbs"].as_array().unwrap();
        for verb in [ACTION_INVOKE_VERB, ACTIONS_LIST_VERB, ACTIONS_DESCRIBE_VERB] {
            assert!(verbs.iter().any(|value| value == verb), "missing {verb}");
        }
    }

    #[test]
    fn discovery_authorises_before_registry_lookup() {
        let mut app = app();
        for command in [ACTIONS_LIST_VERB, ACTIONS_DESCRIBE_VERB] {
            let mut denied = request(command, json!({ "id": "song-open" }));
            denied.from.clear();
            let (rc, body) = dispatch(&mut app, denied);
            assert_eq!(rc, 10);
            assert_eq!(body["error"]["id"], "unregistered_caller");
        }
    }

    #[test]
    fn list_cache_polls_predicates_only_after_a_revision_change() {
        let calls = Arc::new(AtomicUsize::new(0));
        let predicate_calls = Arc::clone(&calls);
        let mut actions = ActionRegistry::new();
        actions
            .register(
                ActionMeta {
                    id: ActionId::from_static("counted"),
                    label: "Counted".into(),
                    args_schema: ArgsSchema::default(),
                    category: None,
                    icon_name: None,
                    description: None,
                    interactive: None,
                    allowed_sources: ActionSources::BUS,
                },
                Arc::new(|_| Ok(())),
                Arc::new(move || {
                    predicate_calls.fetch_add(1, Ordering::Relaxed);
                    true
                }),
            )
            .unwrap();
        let mut app = App::new();
        let (bridge, _peer) = crate::bus::test_bridge("test-app");
        app.insert_resource(bridge)
            .insert_resource(ActionRegistryResource::new(actions))
            .add_plugins(AppPortPlugin::new("Test", "test"))
            .add_plugins(ActionPortPlugin);

        assert_eq!(
            dispatch(&mut app, request(ACTIONS_LIST_VERB, json!({}))).0,
            0
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            dispatch(&mut app, request(ACTIONS_LIST_VERB, json!({}))).0,
            0
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        app.world_mut()
            .resource_mut::<ActionRegistryResource>()
            .mark_enabled_changed();
        assert_eq!(
            dispatch(&mut app, request(ACTIONS_LIST_VERB, json!({}))).0,
            0
        );
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn invocation_rejects_while_modal_capture_is_active() {
        let mut app = app();
        app.world_mut().init_resource::<ModalCapture>();
        app.world_mut().resource_mut::<ModalCapture>().acquire(
            crate::modal_capture::ModalCaptureOwner {
                kind: "test.modal",
                entity: None,
            },
            crate::modal_capture::ModalCaptureLayer(1),
        );
        let (rc, body) = dispatch(
            &mut app,
            request(ACTION_INVOKE_VERB, json!({ "id": "transport.toggle" })),
        );
        assert_eq!(rc, 10);
        assert_eq!(body["error"]["id"], "modal_active");
    }

    #[test]
    fn invocation_rejects_behind_same_frame_interactive_ingress() {
        let mut app = app();
        app.world_mut()
            .resource_mut::<bevy::ecs::message::Messages<ActionRequest>>()
            .write(ActionRequest {
                action: ActionId::from_static("settings"),
                source: Source::Menu,
                args: ActionArgs::new(),
                invocation_focus: None,
            });
        app.update();
        let (rc, body) = dispatch(
            &mut app,
            request(ACTION_INVOKE_VERB, json!({ "id": "transport.toggle" })),
        );
        assert_eq!(rc, 10);
        assert_eq!(body["error"]["id"], "modal_active");
    }

    #[test]
    fn disabled_interactive_ingress_does_not_create_a_capture_barrier() {
        let mut app = app();
        app.world_mut()
            .resource_mut::<bevy::ecs::message::Messages<ActionRequest>>()
            .write(ActionRequest {
                action: ActionId::from_static("settings.disabled"),
                source: Source::Menu,
                args: ActionArgs::new(),
                invocation_focus: None,
            });
        app.update();
        let (rc, body) = dispatch(
            &mut app,
            request(ACTION_INVOKE_VERB, json!({ "id": "transport.toggle" })),
        );
        assert_eq!(rc, 0);
        assert_eq!(body["action"], "transport.toggle");
    }

    #[test]
    fn previous_frame_interactive_ingress_does_not_block_after_a_quiet_frame() {
        let mut app = app();
        let mut messages = app
            .world_mut()
            .resource_mut::<bevy::ecs::message::Messages<ActionRequest>>();
        for _ in 0..2 {
            messages.write(ActionRequest {
                action: ActionId::from_static("settings"),
                source: Source::Menu,
                args: ActionArgs::new(),
                invocation_focus: None,
            });
        }
        app.update();
        let (rc, body) = dispatch(
            &mut app,
            request(ACTION_INVOKE_VERB, json!({ "id": "transport.toggle" })),
        );
        assert_eq!(rc, 10);
        assert_eq!(body["error"]["id"], "modal_active");

        app.update();
        let (rc, body) = dispatch(
            &mut app,
            request(ACTION_INVOKE_VERB, json!({ "id": "transport.toggle" })),
        );
        assert_eq!(rc, 0);
        assert_eq!(body["action"], "transport.toggle");
    }
}
