//! `nspawnd.list/status/start/stop` Bus surface.
//!
//! Executor mutations are registered in flight in delivery order before their
//! task is spawned. A mutation delivered only after a controller has resolved
//! `never_arrived` can still execute late, but it converges towards the same
//! recorded placement desired state on the same fenced owner.

use std::sync::Arc;

use async_trait::async_trait;
use cosmix_bus::{PortReply, bus::BusMessage};
use cosmix_client::{IncomingCommand, SupervisedClient, SupervisedError};
use cosmix_nspawnd::core::{CarriedGrant, InstanceName};
use cosmix_props::namespace::PeerIdentity;
use serde::Deserialize;
use serde_json::{Value, json};
use subtle::ConstantTimeEq;

use crate::citizen::BUS_SERVICE;
use crate::controller::{
    ControllerMutationRequest, ControllerService, ControllerVerb, ExecutorClient, ExecutorReport,
    RemoteOutcome,
};
use crate::service::{ApiError, ApiReply, MutationRequest, NspawnService, REQUEST_SCHEMA};

const MAX_OPERATION_TOKEN_BYTES: usize = 4096;
const MAX_INFLIGHT_COMMANDS: usize = 64;

#[derive(Clone)]
pub struct Authorizer {
    operators: Arc<Vec<String>>,
    expected_token_digest: [u8; 32],
}

impl Authorizer {
    pub fn new(operators: Vec<String>, token: Vec<u8>) -> Result<Self, String> {
        if token.is_empty() || token.len() > MAX_OPERATION_TOKEN_BYTES {
            return Err("operation token must be 1..=4096 bytes".into());
        }
        let expected_token_digest = *blake3::hash(&token).as_bytes();
        Ok(Self {
            operators: Arc::new(operators),
            expected_token_digest,
        })
    }

    pub fn authorize(&self, actor: &str, candidate: &str) -> Result<(), ApiError> {
        if actor.is_empty() || !self.operators.iter().any(|operator| operator == actor) {
            return Err(ApiError::caller(
                "auth_denied",
                "mutating verb requires an allowlisted Bus sender",
            ));
        }
        if candidate.len() > MAX_OPERATION_TOKEN_BYTES {
            return Err(ApiError::caller(
                "invalid_request",
                "operation_token must be at most 4096 bytes",
            ));
        }
        if !constant_time_token_eq(&self.expected_token_digest, candidate.as_bytes()) {
            return Err(ApiError::caller(
                "auth_denied",
                "operation token did not match",
            ));
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusRequest {
    name: InstanceName,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartRequest {
    schema: String,
    name: InstanceName,
    generation: u64,
    grant: CarriedGrant,
    request_id: String,
    operation_token: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StopRequest {
    schema: String,
    name: InstanceName,
    generation: u64,
    grant: CarriedGrant,
    request_id: String,
    operation_token: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestStatusRequest {
    schema: String,
    request_id: String,
    name: InstanceName,
    operation_token: String,
}

pub async fn connect() -> Result<Arc<SupervisedClient>, String> {
    let build = cosmix_buildinfo::build_info!();
    let provenance = cosmix_bus::RegisterProvenance::from_parts(
        build.pkg,
        build.version,
        build.git_sha,
        build.git_dirty,
        build.build_time,
        cosmix_buildinfo::now_rfc3339(),
    );
    SupervisedClient::connect_supervised_with_provenance(
        BUS_SERVICE,
        &cosmix_config::client_helpers::resolve_noded_url(),
        Some(provenance),
    )
    .await
    .map(Arc::new)
    .map_err(|error| format!("connecting supervised Bus client: {error}"))
}

pub async fn run_executor(
    client: Arc<SupervisedClient>,
    service: Arc<NspawnService>,
    authorizer: Authorizer,
) {
    let tracking_service = service.clone();
    run_pump(
        client,
        move |command| pretrack_executor_request(command, &tracking_service),
        move |command| {
            let service = service.clone();
            let authorizer = authorizer.clone();
            async move { dispatch(&command, &service, &authorizer).await }
        },
    )
    .await;
}

pub async fn run_controller(
    client: Arc<SupervisedClient>,
    service: Arc<ControllerService>,
    operators: Authorizer,
    reporters: Authorizer,
) {
    run_pump(
        client,
        |_| (),
        move |command| {
            let service = service.clone();
            let operators = operators.clone();
            let reporters = reporters.clone();
            async move { dispatch_controller(&command, &service, &operators, &reporters).await }
        },
    )
    .await;
}

async fn run_pump<P, G, F, Fut>(client: Arc<SupervisedClient>, prepare: P, dispatch: F)
where
    P: Fn(&IncomingCommand) -> G + Send + Sync + 'static,
    G: Send + 'static,
    F: Fn(Arc<IncomingCommand>) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ApiReply> + Send + 'static,
{
    let Some(mut receiver) = client.incoming() else {
        tracing::error!("Bus incoming receiver unavailable");
        return;
    };
    let dispatch = Arc::new(dispatch);
    let permits = Arc::new(tokio::sync::Semaphore::new(MAX_INFLIGHT_COMMANDS));
    while let Some(command) = receiver.recv().await {
        let permit = match permits.clone().acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => break,
        };
        let client = client.clone();
        let dispatch = dispatch.clone();
        let prepared = prepare(&command);
        let command = Arc::new(command);
        tokio::spawn(async move {
            let _prepared = prepared;
            let reply = dispatch(command.clone()).await;
            if let Err(error) = client
                .respond(&command, reply.rc, &reply.body.to_string())
                .await
            {
                tracing::warn!(error = %error, verb = %command.command, "Bus response send failed");
            }
            drop(permit);
        });
    }
}

fn pretrack_executor_request(
    command: &IncomingCommand,
    service: &NspawnService,
) -> Option<crate::service::InFlightRequestGuard> {
    if !matches!(command.command.as_str(), "nspawnd.start" | "nspawnd.stop") {
        return None;
    }
    let request_id = resolve_args(command)
        .ok()?
        .get("request_id")?
        .as_str()?
        .to_owned();
    Some(service.track_request(&command.from, &request_id))
}

async fn dispatch(
    command: &IncomingCommand,
    service: &NspawnService,
    authorizer: &Authorizer,
) -> ApiReply {
    match command.command.as_str() {
        "nspawnd.list" => match require_empty_object(command) {
            Ok(()) => result_to_reply(service.list().await),
            Err(error) => error.reply(None),
        },
        "nspawnd.status" => {
            let request = match parse_request::<StatusRequest>(command) {
                Ok(request) => request,
                Err(error) => return error.reply(None),
            };
            match service.status(&request.name).await {
                Ok(body) => ApiReply::ok(body),
                Err(error) => error.reply_for(None, Some(&request.name)),
            }
        }
        "nspawnd.start" => {
            let request = match parse_request::<StartRequest>(command) {
                Ok(request) => request,
                Err(error) => return error.reply(None),
            };
            if let Err(error) = validate_schema_and_request_id(&request.schema, &request.request_id)
                .and_then(|()| authorizer.authorize(&command.from, &request.operation_token))
            {
                return error.reply_for(Some(&request.request_id), Some(&request.name));
            }
            let name = request.name.clone();
            match service
                .start(
                    &command.from,
                    MutationRequest {
                        name: request.name,
                        generation: request.generation,
                        request_id: request.request_id.clone(),
                        grant: Some(request.grant),
                    },
                )
                .await
            {
                Ok(reply) => reply,
                Err(error) => error.reply_for(Some(&request.request_id), Some(&name)),
            }
        }
        "nspawnd.stop" => {
            let request = match parse_request::<StopRequest>(command) {
                Ok(request) => request,
                Err(error) => return error.reply(None),
            };
            if let Err(error) = validate_schema_and_request_id(&request.schema, &request.request_id)
                .and_then(|()| authorizer.authorize(&command.from, &request.operation_token))
            {
                return error.reply_for(Some(&request.request_id), Some(&request.name));
            }
            let name = request.name.clone();
            match service
                .stop(
                    &command.from,
                    MutationRequest {
                        name: request.name,
                        generation: request.generation,
                        request_id: request.request_id.clone(),
                        grant: Some(request.grant),
                    },
                )
                .await
            {
                Ok(reply) => reply,
                Err(error) => error.reply_for(Some(&request.request_id), Some(&name)),
            }
        }
        "nspawnd.request.status" => {
            let request = match parse_request::<RequestStatusRequest>(command) {
                Ok(request) => request,
                Err(error) => return error.reply(None),
            };
            if let Err(error) = validate_schema_and_request_id(&request.schema, &request.request_id)
                .and_then(|()| authorizer.authorize(&command.from, &request.operation_token))
            {
                return error.reply_for(Some(&request.request_id), Some(&request.name));
            }
            match service.request_status(&command.from, &request.request_id, &request.name) {
                Ok(body) => ApiReply::ok(body),
                Err(error) => error.reply_for(Some(&request.request_id), Some(&request.name)),
            }
        }
        _ => ApiError::caller(
            "unknown_verb",
            format!("unknown nspawnd verb {:?}", command.command),
        )
        .reply(None),
    }
}

fn result_to_reply(result: Result<Value, ApiError>) -> ApiReply {
    match result {
        Ok(body) => ApiReply::ok(body),
        Err(error) => error.reply(None),
    }
}

fn validate_schema_and_request_id(schema: &str, request_id: &str) -> Result<(), ApiError> {
    if schema != REQUEST_SCHEMA {
        return Err(ApiError::caller(
            "invalid_request",
            format!("unsupported request schema {schema:?}"),
        ));
    }
    if request_id.is_empty() || request_id.len() > 128 {
        return Err(ApiError::caller(
            "invalid_request",
            "request_id must be 1..=128 bytes",
        ));
    }
    Ok(())
}

fn require_empty_object(command: &IncomingCommand) -> Result<(), ApiError> {
    let value = resolve_args(command)?;
    if value.is_null() || value.as_object().is_some_and(serde_json::Map::is_empty) {
        Ok(())
    } else {
        Err(ApiError::caller(
            "invalid_request",
            "nspawnd.list accepts only an empty object",
        ))
    }
}

fn parse_request<T: for<'de> Deserialize<'de>>(command: &IncomingCommand) -> Result<T, ApiError> {
    serde_json::from_value(resolve_args(command)?)
        .map_err(|error| ApiError::caller("invalid_request", error.to_string()))
}

fn resolve_args(command: &IncomingCommand) -> Result<Value, ApiError> {
    if let Some(raw) = command.header("args") {
        return serde_json::from_str(raw)
            .map_err(|error| ApiError::caller("invalid_request", format!("args header: {error}")));
    }
    if !command.body.is_empty() {
        return serde_json::from_str(&command.body)
            .map_err(|error| ApiError::caller("invalid_request", format!("body: {error}")));
    }
    Ok(command.args.clone())
}

fn constant_time_token_eq(expected_digest: &[u8; 32], candidate: &[u8]) -> bool {
    let candidate_digest = blake3::hash(candidate);
    bool::from(expected_digest.ct_eq(candidate_digest.as_bytes()))
}

async fn dispatch_controller(
    command: &IncomingCommand,
    service: &ControllerService,
    operators: &Authorizer,
    reporters: &Authorizer,
) -> ApiReply {
    if let Some(suffix) = command.command.strip_prefix("nspawnd.props.") {
        let mut message = BusMessage::new().with_body(&command.body);
        for (key, value) in &command.headers {
            message.set(key, value);
        }
        let peer = PeerIdentity {
            service_name: Some(command.from.clone()),
            ..PeerIdentity::default()
        };
        let response = service
            .props_router()
            .dispatch(suffix, &message, &peer)
            .await;
        return ApiReply {
            rc: response.rc.clamp(0, 255) as u8,
            body: serde_json::from_str(&response.body)
                .unwrap_or_else(|_| json!({"error":response.body})),
        };
    }
    match command.command.as_str() {
        "nspawnd.ct.list" => match require_empty_object(command) {
            Ok(()) => result_to_reply(service.list().await),
            Err(error) => error.reply(None),
        },
        "nspawnd.ct.status" => {
            let request = match parse_request::<StatusRequest>(command) {
                Ok(value) => value,
                Err(error) => return error.reply(None),
            };
            result_to_reply(service.status(&request.name).await)
        }
        "nspawnd.ct.start" | "nspawnd.ct.stop" | "nspawnd.ct.adopt" => {
            let request = match parse_request::<ControllerMutationRequest>(command) {
                Ok(value) => value,
                Err(error) => return error.reply(None),
            };
            if let Err(error) = operators.authorize(&command.from, &request.operation_token) {
                return error.reply(Some(&request.request_id));
            }
            let verb = match command.command.as_str() {
                "nspawnd.ct.start" => ControllerVerb::Start,
                "nspawnd.ct.stop" => ControllerVerb::Stop,
                _ => ControllerVerb::Adopt,
            };
            let request_id = request.request_id.clone();
            match service.mutate(&command.from, request, verb).await {
                Ok(reply) => reply,
                Err(error) => error.reply(Some(&request_id)),
            }
        }
        "nspawnd.ct.report" => {
            let report = match parse_request::<ExecutorReport>(command) {
                Ok(value) => value,
                Err(error) => return error.reply(None),
            };
            if let Err(error) = reporters.authorize(&command.from, &report.operation_token) {
                return error.reply(report.executor_request_id.as_deref());
            }
            result_to_reply(service.report(&command.from, report).await)
        }
        _ => ApiError::caller(
            "unknown_verb",
            format!("unknown nspawnd verb {:?}", command.command),
        )
        .reply(None),
    }
}

pub struct BusExecutorClient {
    client: Arc<SupervisedClient>,
}

impl BusExecutorClient {
    pub fn new(client: Arc<SupervisedClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl ExecutorClient for BusExecutorClient {
    async fn call(&self, node: &str, verb: &str, body: Value) -> RemoteOutcome {
        let target = format!("nspawnd.{node}");
        match self.client.call_typed(&target, verb, body).await {
            Ok(PortReply::Ok { rc, value }) => RemoteOutcome::Reply { rc, body: value },
            Ok(PortReply::AppError { rc, message }) => RemoteOutcome::Reply {
                rc,
                body: serde_json::from_str(&message)
                    .unwrap_or_else(|_| json!({"ok":false,"message":message})),
            },
            Err(SupervisedError::Disconnected | SupervisedError::ShuttingDown) => {
                RemoteOutcome::RejectedBeforeSend("Bus client is not connected".into())
            }
            Err(error) => RemoteOutcome::Ambiguous(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::ControllerStore;
    use crate::lock::LockManager;
    use crate::service::RC_CALLER;
    use crate::store::StateStore;
    use crate::systemd::{BackendError, MachineEvent, SystemdBackend, Transition};
    use cosmix_nspawnd::core::ObservedInstance;
    use serde_json::json;
    use tokio::sync::{mpsc, oneshot};

    fn command(from: &str, verb: &str, body: Value) -> IncomingCommand {
        IncomingCommand {
            from: from.into(),
            command: verb.into(),
            id: Some("bus-1".into()),
            args: Value::Null,
            body: body.to_string(),
            headers: Default::default(),
        }
    }

    struct UnusedExecutor;

    #[async_trait]
    impl ExecutorClient for UnusedExecutor {
        async fn call(&self, _node: &str, _verb: &str, _body: Value) -> RemoteOutcome {
            RemoteOutcome::Ambiguous("unused".into())
        }
    }

    struct UnusedBackend;

    #[async_trait]
    impl SystemdBackend for UnusedBackend {
        async fn list(&self) -> Result<Vec<ObservedInstance>, BackendError> {
            unreachable!()
        }

        async fn observe(&self, _name: &InstanceName) -> Result<ObservedInstance, BackendError> {
            unreachable!()
        }

        async fn transition(
            &self,
            _name: &InstanceName,
            _transition: Transition,
        ) -> Result<ObservedInstance, BackendError> {
            unreachable!()
        }

        async fn monitor_events(
            &self,
            _sender: mpsc::Sender<MachineEvent>,
            _ready: oneshot::Sender<()>,
        ) -> Result<(), BackendError> {
            unreachable!()
        }
    }

    #[test]
    fn token_comparison_handles_equal_different_and_different_lengths() {
        let digest = *blake3::hash(b"secret").as_bytes();
        assert!(constant_time_token_eq(&digest, b"secret"));
        assert!(!constant_time_token_eq(&digest, b"secrex"));
        assert!(!constant_time_token_eq(&digest, b"secret-long"));
    }

    #[test]
    fn oversized_operation_token_is_invalid_before_comparison() {
        let auth = Authorizer::new(vec!["operator".into()], b"secret".to_vec()).unwrap();
        let oversized = "x".repeat(MAX_OPERATION_TOKEN_BYTES + 1);
        let error = auth.authorize("operator", &oversized).unwrap_err();
        assert_eq!(error.code, "invalid_request");
        assert_eq!(error.rc, RC_CALLER);
    }

    #[test]
    fn authorizer_requires_allowlist_and_token() {
        let auth = Authorizer::new(vec!["operator".into()], b"secret".to_vec()).unwrap();
        assert!(auth.authorize("operator", "secret").is_ok());
        assert_eq!(
            auth.authorize("other", "secret").unwrap_err().code,
            "auth_denied"
        );
        assert_eq!(
            auth.authorize("operator", "wrong").unwrap_err().code,
            "auth_denied"
        );
    }

    #[test]
    fn rejected_operation_token_is_redacted_from_error_envelope() {
        let auth = Authorizer::new(vec!["operator".into()], b"real-secret".to_vec()).unwrap();
        let error = auth.authorize("operator", "candidate-secret").unwrap_err();
        let encoded = error.body(Some("req-1")).to_string();
        assert!(!encoded.contains("candidate-secret"));
        assert!(!encoded.contains("real-secret"));
    }

    #[test]
    fn strict_bus_request_schema_and_rc_envelope() {
        let valid = command(
            "operator",
            "nspawnd.start",
            json!({
                "schema": REQUEST_SCHEMA, "name":"labspoke", "generation":3,
                "grant": {"schema":"cosmix.nspawnd.grant-envelope.v2","name":"labspoke","owner":"alpha","generation":3,"record_version":7,"record_state":"placed","record_updated":"2026-08-09T00:00:00Z"},
                "request_id":"req-1", "operation_token":"secret"
            }),
        );
        let parsed: StartRequest = parse_request(&valid).unwrap();
        assert_eq!(parsed.generation, 3);

        let invalid = command(
            "operator",
            "nspawnd.start",
            json!({
                "schema": REQUEST_SCHEMA, "name":"labspoke", "generation":3,
                "grant": {"schema":"cosmix.nspawnd.grant-envelope.v2","name":"labspoke","owner":"alpha","generation":3,"record_version":7,"record_state":"placed","record_updated":"2026-08-09T00:00:00Z"},
                "request_id":"req-1", "operation_token":"secret", "extra":true
            }),
        );
        let error = match parse_request::<StartRequest>(&invalid) {
            Ok(_) => panic!("unknown field must be rejected"),
            Err(error) => error,
        };
        assert_eq!(error.rc, RC_CALLER);

        let legacy_stop = command(
            "operator",
            "nspawnd.stop",
            json!({
                "schema": REQUEST_SCHEMA, "name":"labspoke", "expected_generation":3,
                "grant": {"schema":"cosmix.nspawnd.grant-envelope.v2","name":"labspoke","owner":"alpha","generation":3,"record_version":7,"record_state":"placed","record_updated":"2026-08-09T00:00:00Z"},
                "request_id":"req-2", "operation_token":"secret"
            }),
        );
        assert!(parse_request::<StopRequest>(&legacy_stop).is_err());

        let error = ApiError::backend("systemd_error", "down", true).reply(Some("req-1"));
        assert_eq!(error.rc, 20);
        assert_eq!(error.body["error_code"], "systemd_error");
        assert_eq!(
            ApiError::caller("invalid_request", "bad").reply(None).rc,
            10
        );
    }

    #[tokio::test]
    async fn parsed_controller_mutation_errors_retain_request_id() {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(ControllerStore::open(&temp.path().join("controller.db")).unwrap());
        let service = ControllerService::new(
            store,
            Arc::new(UnusedExecutor),
            "operation-secret".into(),
            Vec::<String>::new(),
        );
        let operators =
            Authorizer::new(vec!["operator".into()], b"operator-secret".to_vec()).unwrap();
        let reporters = Authorizer::new(Vec::new(), b"operation-secret".to_vec()).unwrap();
        let reply = dispatch_controller(
            &command(
                "operator",
                "nspawnd.ct.adopt",
                json!({
                    "schema":crate::controller::CONTROLLER_REQUEST_SCHEMA,
                    "name":"demo",
                    "owner":"alpha",
                    "generation":2,
                    "if_version":0,
                    "request_id":"ct-correlated",
                    "operation_token":"operator-secret",
                }),
            ),
            &service,
            &operators,
            &reporters,
        )
        .await;
        assert_eq!(reply.body["request_id"], "ct-correlated");
        assert_eq!(reply.body["error_code"], "adopt_fencing_unavailable");
    }

    #[test]
    fn pump_pretracks_delivered_mutation_before_dispatch_task() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(temp.path().join("state"), temp.path().join("legacy"));
        store.ensure_layout().unwrap();
        let service = NspawnService::new(
            "alpha".into(),
            store,
            LockManager::new(temp.path().join("locks")),
            Arc::new(UnusedBackend),
        );
        let command = command(
            "bridge-controller",
            "nspawnd.start",
            json!({"request_id":"delivered-before-probe"}),
        );
        let guard = pretrack_executor_request(&command, &service).unwrap();
        assert_eq!(
            service
                .request_status(
                    "bridge-controller",
                    "delivered-before-probe",
                    &InstanceName::parse("demo").unwrap(),
                )
                .unwrap()["in_flight"],
            true
        );
        drop(guard);
        assert_eq!(
            service
                .request_status(
                    "bridge-controller",
                    "delivered-before-probe",
                    &InstanceName::parse("demo").unwrap(),
                )
                .unwrap()["in_flight"],
            false
        );
    }
}
