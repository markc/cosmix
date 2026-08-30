//! Pure placement controller: SPEC-12 state plus generation-grant dispatch.
//!
//! Executor replies are terminal only when they carry the expected nspawnd
//! success or error schema; mesh-manufactured and malformed replies remain
//! outcome-unknown. Adoption is fenced across the executor fleet listed by
//! the controller's reporter allowlist. Reports resolve correlated operations
//! only from their additive terminal-operation fields, with observation used
//! solely to resolve timeout/interrupted outcomes that have converged.
//!
//! # Fencing assumptions
//!
//! Adopt fencing is sound only when `COSMIX_NSPAWND_REPORTERS` lists every node
//! capable of running instances and exactly one controller using one database
//! is active. A grant on a roster-external node is invisible to fencing. Dead
//! node migration and cross-node grant revocation remain part of the C3
//! migration saga; recovery therefore fails closed while an owner is unreachable.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use cosmix_nspawnd::core::{CARRIED_GRANT_SCHEMA, CarriedGrant, DesiredState, InstanceName};
use cosmix_props::bus::mutation::PropsRouter;
use cosmix_props::capability::{Capability, CapabilitySet};
use cosmix_props::hooks::WriteOrigin;
use cosmix_props::namespace::{
    AuthPolicy, Cardinality, FieldSchema, FieldType, NamespaceName, NamespaceSpec, PropertySchema,
    StorageBackendKind,
};
use cosmix_props::record::{Actor, Record, RecordKey, Version};
use cosmix_props::runtime::{Runtime, RuntimeError, SetOpts};
use cosmix_props::sqlite::{JsonValuesMapping, SqliteStore};
use cosmix_props::store::{MergeMode, StoreError};
use cosmix_props::value::PropValue;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::service::{ApiError, ApiReply, RC_BACKEND, RC_OK, REQUEST_SCHEMA};

pub const PLACEMENT_SCHEMA: &str = "cosmix.nspawnd.placement.v1";
pub const OBSERVATION_SCHEMA: &str = "cosmix.nspawnd.observation.v1";
pub const CONTROLLER_OPERATION_SCHEMA: &str = "cosmix.nspawnd.controller-operation.v1";
pub const CONTROLLER_REQUEST_SCHEMA: &str = "cosmix.nspawnd.ct-request.v1";
pub const REPORT_SCHEMA: &str = "cosmix.nspawnd.report.v1";
const EXECUTOR_ERROR_SCHEMA: &str = "cosmix.nspawnd.error.v1";
const EXECUTOR_OPERATION_SCHEMA: &str = "cosmix.nspawnd.operation.v1";
const EXECUTOR_STATUS_SCHEMA: &str = "cosmix.nspawnd.status.v1";
const EXECUTOR_REQUEST_STATUS_SCHEMA: &str = "cosmix.nspawnd.request-status.v1";
/// A missing, non-in-flight executor request is conclusive only after more
/// than two complete 30-second mesh timeout windows.
const NEVER_ARRIVED_GRACE: Duration = Duration::from_secs(120);
/// Keep never-arrived classification far inside the executor's 24-hour/500-op
/// replay retention horizon; beyond this cap, absence may mean eviction.
const NEVER_ARRIVED_MAX_AGE: Duration = Duration::from_secs(60 * 60);
/// Outcome-unknown requests get a generous interval for executor reconciliation
/// before one authoritative observation closes them by convergence.
const CONVERGENCE_WINDOW: Duration = Duration::from_secs(15 * 60);

const INSTANCES_NS: &str = "instances";
const OBSERVATIONS_NS: &str = "observations";
const OPERATIONS_NS: &str = "operations";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlacementRecord {
    pub schema: String,
    pub name: InstanceName,
    pub owner: String,
    pub generation: u64,
    /// Immutable evidence for this generation grant. Placement CAS version
    /// continues to advance as desired state changes.
    pub grant_record_version: u64,
    pub grant_record_updated: String,
    pub desired: DesiredState,
    pub state: String,
    pub op: Option<String>,
    pub prepared_by: Option<String>,
    pub intent_hash: Option<String>,
    pub updated_at: String,
}

impl PlacementRecord {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != PLACEMENT_SCHEMA {
            return Err(format!("unsupported placement schema {:?}", self.schema));
        }
        if self.owner.is_empty()
            || self.generation == 0
            || self.grant_record_version == 0
            || self.grant_record_updated.is_empty()
            || self.updated_at.is_empty()
        {
            return Err("owner and updated_at must be non-empty; generation must be >= 1".into());
        }
        if self.state != "placed" {
            return Err("placement state must be placed".into());
        }
        if self.prepared_by.is_some() != self.intent_hash.is_some() {
            return Err("prepared_by and intent_hash must be present together".into());
        }
        Ok(())
    }

    fn grant(&self) -> CarriedGrant {
        CarriedGrant {
            schema: CARRIED_GRANT_SCHEMA.into(),
            name: self.name.clone(),
            owner: self.owner.clone(),
            generation: self.generation,
            record_version: self.grant_record_version,
            record_state: self.state.clone(),
            record_updated: self.grant_record_updated.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationRecord {
    pub schema: String,
    pub name: InstanceName,
    pub node: String,
    pub generation: u64,
    pub state: String,
    pub image_present: bool,
    pub unit_active: String,
    pub executor_request_id: Option<String>,
    pub executor_op_id: Option<String>,
    pub reported_at: String,
    pub received_at: String,
}

impl ObservationRecord {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != OBSERVATION_SCHEMA {
            return Err(format!("unsupported observation schema {:?}", self.schema));
        }
        if self.node.is_empty() || self.generation == 0 {
            return Err("node must be non-empty and generation must be >= 1".into());
        }
        if !matches!(self.state.as_str(), "running" | "stopped" | "absent") {
            return Err("observation state must be running, stopped, or absent".into());
        }
        validate_timestamp(&self.reported_at)?;
        validate_timestamp(&self.received_at)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ControllerVerb {
    Start,
    Stop,
    Adopt,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ControllerPhase {
    Prepared,
    Ready,
    Dispatching,
    Unknown,
    Succeeded,
    Failed,
}

impl ControllerPhase {
    pub fn terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ControllerOperation {
    pub schema: String,
    pub claim_key: String,
    pub op_id: String,
    pub actor: String,
    pub request_id: String,
    pub request_hash: String,
    /// Explicit durable marker for adopt's one-shot fleet preflight. Missing
    /// on older records and therefore defaults to non-adopt.
    #[serde(default)]
    pub is_adopt: bool,
    pub verb: ControllerVerb,
    pub name: InstanceName,
    pub target: String,
    pub generation: u64,
    pub desired: DesiredState,
    pub executor_request_id: String,
    pub phase: ControllerPhase,
    pub placement_version_before: u64,
    pub placement_version_after: Option<u64>,
    pub executor_op_id: Option<String>,
    pub response_rc: Option<u8>,
    pub response_body: Option<Value>,
    pub started_at: String,
    pub completed_at: Option<String>,
}

impl ControllerOperation {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != CONTROLLER_OPERATION_SCHEMA {
            return Err(format!("unsupported operation schema {:?}", self.schema));
        }
        if self.claim_key.is_empty()
            || self.op_id.is_empty()
            || self.actor.is_empty()
            || self.request_id.is_empty()
            || self.request_hash.is_empty()
            || self.target.is_empty()
            || self.executor_request_id.is_empty()
            || self.generation == 0
        {
            return Err("controller operation identity fields must be non-empty".into());
        }
        validate_timestamp(&self.started_at)?;
        if self.phase.terminal() != self.completed_at.is_some() {
            return Err("terminal phase and completed_at must agree".into());
        }
        if let Some(at) = &self.completed_at {
            validate_timestamp(at)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControllerMutationRequest {
    pub schema: String,
    pub name: InstanceName,
    pub owner: Option<String>,
    pub generation: Option<u64>,
    pub if_version: u64,
    pub request_id: String,
    pub operation_token: String,
}

impl ControllerMutationRequest {
    pub fn validate(&self, verb: ControllerVerb) -> Result<(), ApiError> {
        if self.schema != CONTROLLER_REQUEST_SCHEMA {
            return Err(ApiError::caller(
                "invalid_request",
                "unsupported controller request schema",
            ));
        }
        if self.request_id.is_empty() || self.request_id.len() > 128 {
            return Err(ApiError::caller(
                "invalid_request",
                "request_id must be 1..=128 bytes",
            ));
        }
        if matches!(verb, ControllerVerb::Adopt)
            && (self.owner.as_deref().is_none_or(str::is_empty)
                || self.generation.is_none_or(|value| value == 0))
        {
            return Err(ApiError::caller(
                "invalid_request",
                "adopt requires owner and generation >= 1",
            ));
        }
        if !matches!(verb, ControllerVerb::Adopt)
            && (self.owner.is_some() || self.generation.is_some())
        {
            return Err(ApiError::caller(
                "invalid_request",
                "owner and generation are adopt-only fields",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutorReport {
    pub schema: String,
    pub name: InstanceName,
    pub node: String,
    pub generation: u64,
    pub state: String,
    pub image_present: bool,
    pub unit_active: String,
    pub executor_request_id: Option<String>,
    pub executor_op_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor_operation_state: Option<ExecutorTerminalState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor_error_code: Option<String>,
    pub reported_at: String,
    pub operation_token: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutorTerminalState {
    Succeeded,
    Failed,
}

impl ExecutorReport {
    fn validate(&self) -> Result<(), String> {
        if self.schema != REPORT_SCHEMA {
            return Err("unsupported report schema".into());
        }
        if self.node.is_empty() || self.generation == 0 {
            return Err("report node must be non-empty and generation must be >= 1".into());
        }
        if !matches!(self.state.as_str(), "running" | "stopped" | "absent") {
            return Err("report state must be running, stopped, or absent".into());
        }
        if self.executor_error_code.is_some()
            != matches!(
                self.executor_operation_state,
                Some(ExecutorTerminalState::Failed)
            )
        {
            return Err("executor_error_code must be present exactly for failed operations".into());
        }
        validate_timestamp(&self.reported_at)
    }
}

#[derive(Clone, Debug)]
pub enum RemoteOutcome {
    Reply { rc: u8, body: Value },
    RejectedBeforeSend(String),
    Ambiguous(String),
}

#[async_trait]
pub trait ExecutorClient: Send + Sync {
    async fn call(&self, node: &str, verb: &str, body: Value) -> RemoteOutcome;
}

pub struct ControllerStore {
    pub router: Arc<PropsRouter>,
    instances: Arc<Runtime>,
    observations: Arc<Runtime>,
    operations: Arc<Runtime>,
}

impl ControllerStore {
    pub fn open(path: &Path) -> Result<Self, String> {
        let conn = rusqlite::Connection::open(path)
            .map_err(|error| format!("opening controller database {}: {error}", path.display()))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;",
        )
        .map_err(|error| format!("configuring controller database: {error}"))?;
        let store = Arc::new(SqliteStore::new("nspawnd", conn).map_err(store_string)?);
        let instances = register_runtime(&store, INSTANCES_NS, "name")?;
        let observations = register_runtime(&store, OBSERVATIONS_NS, "name")?;
        let operations = register_runtime(&store, OPERATIONS_NS, "claim_key")?;
        let mut router = PropsRouter::new("nspawnd");
        router.register(instances.clone())?;
        router.register(observations.clone())?;
        router.register(operations.clone())?;
        Ok(Self {
            router: Arc::new(router),
            instances,
            observations,
            operations,
        })
    }

    pub async fn get_placement(
        &self,
        name: &InstanceName,
    ) -> Result<Option<(PlacementRecord, u64)>, ApiError> {
        get_typed(&self.instances, INSTANCES_NS, name.as_str()).await
    }

    pub async fn list_placements(&self) -> Result<Vec<(PlacementRecord, u64)>, ApiError> {
        list_typed(&self.instances, INSTANCES_NS).await
    }

    pub async fn set_placement(
        &self,
        value: &PlacementRecord,
        expected: u64,
        cause: &str,
    ) -> Result<u64, ApiError> {
        value
            .validate()
            .map_err(|error| ApiError::caller("invalid_placement", error))?;
        set_typed(
            &self.instances,
            INSTANCES_NS,
            value.name.as_str(),
            value,
            expected,
            cause,
        )
        .await
    }

    pub async fn get_observation(
        &self,
        name: &InstanceName,
    ) -> Result<Option<(ObservationRecord, u64)>, ApiError> {
        get_typed(&self.observations, OBSERVATIONS_NS, name.as_str()).await
    }

    pub async fn set_observation(
        &self,
        value: &ObservationRecord,
        expected: u64,
    ) -> Result<u64, ApiError> {
        value
            .validate()
            .map_err(|error| ApiError::caller("invalid_observation", error))?;
        set_typed(
            &self.observations,
            OBSERVATIONS_NS,
            value.name.as_str(),
            value,
            expected,
            "executor-report",
        )
        .await
    }

    pub async fn get_operation(
        &self,
        key: &str,
    ) -> Result<Option<(ControllerOperation, u64)>, ApiError> {
        get_typed(&self.operations, OPERATIONS_NS, key).await
    }

    pub async fn list_operations(&self) -> Result<Vec<(ControllerOperation, u64)>, ApiError> {
        list_typed(&self.operations, OPERATIONS_NS).await
    }

    pub async fn set_operation(
        &self,
        value: &ControllerOperation,
        expected: u64,
    ) -> Result<u64, ApiError> {
        value
            .validate()
            .map_err(|error| ApiError::caller("invalid_operation", error))?;
        set_typed(
            &self.operations,
            OPERATIONS_NS,
            &value.claim_key,
            value,
            expected,
            &value.request_id,
        )
        .await
    }
}

fn register_runtime(
    store: &Arc<SqliteStore>,
    name: &str,
    key: &str,
) -> Result<Arc<Runtime>, String> {
    let namespace = NamespaceName::new(name).map_err(|error| error.to_string())?;
    let mut spec = NamespaceSpec::new(
        namespace.clone(),
        schema_for(name),
        Cardinality::Collection {
            primary_key_field: key.into(),
        },
        StorageBackendKind::SqliteTable {
            table: "__props_values".into(),
        },
    );
    spec.require_version = true;
    let fqn = format!("nspawnd.{name}");
    spec.auth = AuthPolicy::new(move |peer| {
        if peer.service_name.is_none() {
            return CapabilitySet::empty();
        }
        [
            Capability::new(format!("props.read:{fqn}")),
            Capability::new(format!("props.describe:{fqn}:public")),
            Capability::new(format!("props.audit:{fqn}")),
        ]
        .into_iter()
        .collect()
    });
    store
        .register_namespace(&spec, Arc::new(JsonValuesMapping::new(namespace)))
        .map_err(store_string)?;
    Ok(Arc::new(Runtime::new("nspawnd", spec, store.clone())))
}

fn schema_for(namespace: &str) -> PropertySchema {
    let string = || FieldType::String;
    let optional_string = || FieldType::Option {
        inner: Box::new(FieldType::String),
    };
    let fields = match namespace {
        INSTANCES_NS => vec![
            field("schema", string()),
            field("name", string()),
            field("owner", string()),
            field("generation", FieldType::U64),
            field("grant_record_version", FieldType::U64),
            field("grant_record_updated", string()),
            field("desired", string()),
            field("state", string()),
            field("op", optional_string()),
            field("prepared_by", optional_string()),
            field("intent_hash", optional_string()),
            field("updated_at", string()),
        ],
        OBSERVATIONS_NS => vec![
            field("schema", string()),
            field("name", string()),
            field("node", string()),
            field("generation", FieldType::U64),
            field("state", string()),
            field("image_present", FieldType::Bool),
            field("unit_active", string()),
            field("executor_request_id", optional_string()),
            field("executor_op_id", optional_string()),
            field("reported_at", string()),
            field("received_at", string()),
        ],
        OPERATIONS_NS => vec![
            field("schema", string()),
            field("claim_key", string()),
            field("op_id", string()),
            field("actor", string()),
            field("request_id", string()),
            field("request_hash", string()),
            field("is_adopt", FieldType::Bool),
            field("verb", string()),
            field("name", string()),
            field("target", string()),
            field("generation", FieldType::U64),
            field("desired", string()),
            field("executor_request_id", string()),
            field("phase", string()),
            field("placement_version_before", FieldType::U64),
            field(
                "placement_version_after",
                FieldType::Option {
                    inner: Box::new(FieldType::U64),
                },
            ),
            field("executor_op_id", optional_string()),
            field(
                "response_rc",
                FieldType::Option {
                    inner: Box::new(FieldType::U64),
                },
            ),
            field("started_at", string()),
            field("completed_at", optional_string()),
        ],
        _ => Vec::new(),
    };
    PropertySchema::new(fields)
}

fn field(name: &str, ty: FieldType) -> FieldSchema {
    FieldSchema {
        name: name.into(),
        ty,
        default: None,
        secret: false,
        help: String::new(),
        since: None,
        until: None,
        validators: Vec::new(),
    }
}

async fn get_typed<T: DeserializeOwned>(
    runtime: &Runtime,
    ns: &str,
    key: &str,
) -> Result<Option<(T, u64)>, ApiError> {
    let record_key = RecordKey::collection(NamespaceName::new(ns).unwrap(), key);
    match runtime.store().get(&record_key).await {
        Ok(snapshot) => decode_record(snapshot.value).map(Some),
        Err(StoreError::NotFound) => Ok(None),
        Err(error) => Err(props_error(error)),
    }
}

async fn list_typed<T: DeserializeOwned>(
    runtime: &Runtime,
    ns: &str,
) -> Result<Vec<(T, u64)>, ApiError> {
    let namespace = NamespaceName::new(ns).unwrap();
    runtime
        .store()
        .list(&namespace)
        .await
        .map_err(props_error)?
        .value
        .into_iter()
        .map(decode_record)
        .collect()
}

fn decode_record<T: DeserializeOwned>(record: Record) -> Result<(T, u64), ApiError> {
    let value = serde_json::to_value(record.value)
        .map_err(|error| ApiError::backend("storage_error", error.to_string(), false))?;
    let typed = serde_json::from_value(value)
        .map_err(|error| ApiError::backend("state_corrupt", error.to_string(), false))?;
    Ok((typed, record.version.0))
}

async fn set_typed<T: Serialize>(
    runtime: &Runtime,
    ns: &str,
    key: &str,
    value: &T,
    expected: u64,
    cause: &str,
) -> Result<u64, ApiError> {
    let json = serde_json::to_value(value)
        .map_err(|error| ApiError::caller("invalid_request", error.to_string()))?;
    let prop: PropValue = serde_json::from_value(json)
        .map_err(|error| ApiError::caller("invalid_request", error.to_string()))?;
    let outcome = runtime
        .set_with_origin(
            RecordKey::collection(NamespaceName::new(ns).unwrap(), key),
            prop,
            SetOpts {
                expected_version: Some(Version(expected)),
                merge: MergeMode::Replace,
                actor: Actor::daemon_complete("nspawnd"),
                cause: Some(cause.into()),
                ts_ms: Utc::now().timestamp_millis(),
            },
            WriteOrigin::backend(),
        )
        .await
        .map_err(runtime_error)?;
    Ok(outcome.set_event.version.0)
}

fn runtime_error(error: RuntimeError) -> ApiError {
    match error {
        RuntimeError::Store(error) => props_error(error),
        RuntimeError::Hook(error) => ApiError::backend("storage_error", error.to_string(), false),
    }
}

fn props_error(error: StoreError) -> ApiError {
    match error {
        StoreError::VersionMismatch { .. } | StoreError::Conflict { .. } => {
            ApiError::caller("version_mismatch", error.to_string())
        }
        StoreError::NotFound => ApiError::caller("not_found", error.to_string()),
        _ => ApiError::backend("storage_error", error.to_string(), true),
    }
}

fn store_string(error: StoreError) -> String {
    error.to_string()
}

fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn validate_timestamp(value: &str) -> Result<(), String> {
    let parsed = DateTime::parse_from_rfc3339(value)
        .map_err(|error| format!("invalid RFC3339 timestamp: {error}"))?;
    if parsed
        .with_timezone(&Utc)
        .to_rfc3339_opts(SecondsFormat::Secs, true)
        != value
    {
        return Err("timestamp must be canonical UTC whole-second RFC3339 ending in Z".into());
    }
    Ok(())
}

pub struct ControllerService {
    store: Arc<ControllerStore>,
    executor: Arc<dyn ExecutorClient>,
    operation_token: String,
    executor_roster: BTreeSet<String>,
    mutation_lock: Mutex<()>,
}

impl ControllerService {
    pub fn new(
        store: Arc<ControllerStore>,
        executor: Arc<dyn ExecutorClient>,
        operation_token: String,
        executor_roster: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            store,
            executor,
            operation_token,
            executor_roster: executor_roster.into_iter().collect(),
            mutation_lock: Mutex::new(()),
        }
    }

    pub fn props_router(&self) -> Arc<PropsRouter> {
        self.store.router.clone()
    }

    pub async fn list(&self) -> Result<Value, ApiError> {
        let mut instances = Vec::new();
        for (placement, version) in self.store.list_placements().await? {
            let observed = self
                .store
                .get_observation(&placement.name)
                .await?
                .map(|value| value.0);
            instances
                .push(json!({"placement": placement, "version": version, "observed": observed}));
        }
        Ok(
            json!({"schema":"cosmix.nspawnd.ct-list.v1","ok":true,"instances":instances,"observed_at":now()}),
        )
    }

    pub async fn status(&self, name: &InstanceName) -> Result<Value, ApiError> {
        let placement = self.store.get_placement(name).await?;
        let observation = self.store.get_observation(name).await?;
        Ok(json!({
            "schema":"cosmix.nspawnd.ct-status.v1", "ok":true, "name":name,
            "found":placement.is_some(),
            "placement":placement.as_ref().map(|value| &value.0),
            "version":placement.as_ref().map(|value| value.1),
            "observed":observation.map(|value| value.0), "observed_at":now()
        }))
    }

    pub async fn mutate(
        &self,
        actor: &str,
        request: ControllerMutationRequest,
        verb: ControllerVerb,
    ) -> Result<ApiReply, ApiError> {
        request.validate(verb)?;
        let _guard = self.mutation_lock.lock().await;
        let claim_key = claim_key(actor, &request.request_id);
        let intent = json!({
            "schema":request.schema,"verb":verb,"name":request.name,"owner":request.owner,
            "generation":request.generation,"if_version":request.if_version,"request_id":request.request_id
        });
        let request_hash = blake3::hash(&serde_json::to_vec(&intent).unwrap())
            .to_hex()
            .to_string();
        if let Some((existing, _)) = self.store.get_operation(&claim_key).await? {
            return replay_controller(existing, &request_hash);
        }

        let current = self.store.get_placement(&request.name).await?;
        let (owner, generation, desired) = match verb {
            ControllerVerb::Start | ControllerVerb::Stop => {
                let (placement, version) = current.as_ref().ok_or_else(|| {
                    ApiError::caller("not_found", "placement does not exist; adopt it first")
                })?;
                if *version != request.if_version {
                    return Err(ApiError::caller(
                        "version_mismatch",
                        format!(
                            "if_version {} does not match {}",
                            request.if_version, version
                        ),
                    ));
                }
                if placement.op.is_some() {
                    return Err(ApiError::caller(
                        "operation_in_progress",
                        "placement already has an unresolved operation",
                    ));
                }
                (
                    placement.owner.clone(),
                    placement.generation,
                    if verb == ControllerVerb::Start {
                        DesiredState::Running
                    } else {
                        DesiredState::Stopped
                    },
                )
            }
            ControllerVerb::Adopt => {
                if current.is_some() || request.if_version != 0 {
                    return Err(ApiError::caller(
                        "version_mismatch",
                        "adopt requires an absent placement and if_version 0",
                    ));
                }
                let owner = request.owner.clone().unwrap();
                let generation = request.generation.unwrap();
                let desired = self
                    .adopt_preflight(&owner, &request.name, generation)
                    .await?;
                (owner, generation, desired)
            }
        };
        let op_id = ulid::Ulid::new().to_string();
        let executor_request_id = format!("ct-{op_id}");
        let mut operation = ControllerOperation {
            schema: CONTROLLER_OPERATION_SCHEMA.into(),
            claim_key: claim_key.clone(),
            op_id: op_id.clone(),
            actor: actor.into(),
            request_id: request.request_id.clone(),
            request_hash: request_hash.clone(),
            is_adopt: verb == ControllerVerb::Adopt,
            verb,
            name: request.name.clone(),
            target: owner.clone(),
            generation,
            desired,
            executor_request_id,
            phase: ControllerPhase::Prepared,
            placement_version_before: request.if_version,
            placement_version_after: None,
            executor_op_id: None,
            response_rc: None,
            response_body: None,
            started_at: now(),
            completed_at: None,
        };
        // The replay key is durable before the placement CAS.
        self.store.set_operation(&operation, 0).await?;

        let mut placement = current.map_or_else(
            || PlacementRecord {
                schema: PLACEMENT_SCHEMA.into(),
                name: request.name,
                owner,
                generation,
                grant_record_version: request.if_version + 1,
                grant_record_updated: now(),
                desired,
                state: "placed".into(),
                op: Some(op_id.clone()),
                prepared_by: Some(claim_key.clone()),
                intent_hash: Some(request_hash.clone()),
                updated_at: now(),
            },
            |value| {
                let mut placement = value.0;
                placement.desired = desired;
                placement.op = Some(op_id.clone());
                placement.prepared_by = Some(claim_key.clone());
                placement.intent_hash = Some(request_hash.clone());
                placement.updated_at = now();
                placement
            },
        );
        let placement_version = match self
            .store
            .set_placement(&placement, request.if_version, &request.request_id)
            .await
        {
            Ok(version) => version,
            Err(error) => {
                operation.phase = ControllerPhase::Failed;
                operation.response_rc = Some(error.rc);
                operation.response_body = Some(error.body(Some(&request.request_id)));
                operation.completed_at = Some(now());
                self.store.set_operation(&operation, 1).await?;
                return Err(error);
            }
        };
        operation.placement_version_after = Some(placement_version);
        operation.phase = ControllerPhase::Ready;
        self.store.set_operation(&operation, 1).await?;
        self.dispatch(&mut operation, 2, &mut placement, placement_version)
            .await
    }

    async fn adopt_preflight(
        &self,
        owner: &str,
        name: &InstanceName,
        generation: u64,
    ) -> Result<DesiredState, ApiError> {
        if self.executor_roster.is_empty() {
            return Err(ApiError::backend(
                "adopt_fencing_unavailable",
                "controller cannot fence adopt without a reporter roster",
                true,
            ));
        }
        if !self.executor_roster.contains(owner) {
            return Err(ApiError::caller(
                "owner_not_in_roster",
                format!("adopt target {owner:?} is not in the reporter-listed executor fleet"),
            ));
        }

        // Launch the complete fleet probe before evaluating any result so a
        // conflict on one executor never prevents fencing checks elsewhere.
        let probes = futures_util::future::join_all(self.executor_roster.iter().map(|node| {
            let body = json!({"name":name});
            async move { (node, self.executor.call(node, "nspawnd.status", body).await) }
        }))
        .await;
        let mut target_desired = None;
        for (node, outcome) in probes {
            let body = match outcome {
                RemoteOutcome::Reply { rc: RC_OK, body }
                    if valid_executor_status_reply(&body, name) =>
                {
                    body
                }
                RemoteOutcome::Reply { rc: RC_OK, body } => {
                    return Err(ApiError::backend(
                        "executor_status_unknown",
                        format!("executor {node:?} returned malformed status: {body}"),
                        true,
                    ));
                }
                RemoteOutcome::Reply { rc, body } if is_executor_error_for(&body, None, name) => {
                    return Err(ApiError::backend(
                        "executor_status_failed",
                        format!("executor {node:?} status returned rc {rc}: {body}"),
                        body.get("retryable")
                            .and_then(Value::as_bool)
                            .unwrap_or(rc >= RC_BACKEND),
                    ));
                }
                RemoteOutcome::Reply { rc, body } => {
                    return Err(ApiError::backend(
                        "executor_status_unknown",
                        format!("executor {node:?} returned untrusted status rc {rc}: {body}"),
                        true,
                    ));
                }
                RemoteOutcome::RejectedBeforeSend(message) => {
                    return Err(ApiError::backend(
                        "executor_unavailable",
                        format!("executor {node:?}: {message}"),
                        true,
                    ));
                }
                RemoteOutcome::Ambiguous(message) => {
                    return Err(ApiError::backend(
                        "executor_status_unknown",
                        format!("executor {node:?}: {message}"),
                        true,
                    ));
                }
            };
            let managed = body["managed"].as_bool().unwrap();
            if node.as_str() != owner && (managed || body["observed"] == "running") {
                return Err(ApiError::caller(
                    if managed {
                        "conflicting_owner"
                    } else {
                        "conflicting_running"
                    },
                    format!(
                        "executor {node:?} already {} instance {name}",
                        if managed { "manages" } else { "runs" }
                    ),
                ));
            }
            if let Some(installed) = body["grant_generation"].as_u64()
                && installed >= generation
            {
                return Err(ApiError::caller(
                    "generation_stale",
                    format!(
                        "adopt generation {generation} must be greater than executor {node:?} grant {installed}"
                    ),
                ));
            }
            if node.as_str() == owner {
                if !managed {
                    return Err(ApiError::caller(
                        "not_managed",
                        "adopt requires an existing executor-managed instance on the target",
                    ));
                }
                if !body["current_operation"].is_null() {
                    return Err(ApiError::caller(
                        "busy",
                        "adopt refuses a target executor operation in progress",
                    ));
                }
                target_desired = Some(if body["observed"] == "running" {
                    DesiredState::Running
                } else {
                    DesiredState::Stopped
                });
            }
        }
        target_desired.ok_or_else(|| {
            ApiError::backend(
                "adopt_fencing_unavailable",
                "reporter roster did not yield a target executor status",
                true,
            )
        })
    }

    async fn dispatch(
        &self,
        operation: &mut ControllerOperation,
        operation_version: u64,
        placement: &mut PlacementRecord,
        _placement_version: u64,
    ) -> Result<ApiReply, ApiError> {
        operation.phase = ControllerPhase::Dispatching;
        self.store
            .set_operation(operation, operation_version)
            .await?;
        let grant = placement.grant();
        let verb = if operation.desired == DesiredState::Running {
            "nspawnd.start"
        } else {
            "nspawnd.stop"
        };
        let body = json!({
            "schema":REQUEST_SCHEMA,"name":operation.name,"generation":operation.generation,
            "grant":grant,"request_id":operation.executor_request_id,"operation_token":self.operation_token
        });
        let remote = self.executor.call(&operation.target, verb, body).await;
        let (phase, rc, reply_body) = classify_remote(operation, remote);
        let retryable = reply_retryable(phase, &reply_body);
        let final_body = if phase == ControllerPhase::Succeeded {
            controller_reply(operation, reply_body)
        } else {
            json!({
                "schema":"cosmix.nspawnd.ct-error.v1","ok":false,
                "request_id":operation.request_id,"op_id":operation.op_id,
                "name":operation.name,"error_code":if phase == ControllerPhase::Unknown { "executor_unknown" } else { "executor_rejected" },
                "retryable":retryable,"executor":reply_body
            })
        };
        operation.phase = phase;
        operation.response_rc = Some(rc);
        operation.executor_op_id = final_body
            .pointer("/executor/op_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        operation.response_body = Some(final_body.clone());
        if phase.terminal() {
            operation.completed_at = Some(now());
        }
        self.store
            .set_operation(operation, operation_version + 1)
            .await?;
        if phase.terminal() {
            self.clear_placement_marker(operation).await?;
        }
        Ok(ApiReply {
            rc,
            body: final_body,
        })
    }

    pub async fn report(&self, actor: &str, report: ExecutorReport) -> Result<Value, ApiError> {
        report
            .validate()
            .map_err(|error| ApiError::caller("invalid_request", error))?;
        let expected_actor = format!("bridge-{}", report.node);
        if actor != expected_actor {
            return Err(ApiError::caller(
                "auth_denied",
                "report actor does not match report node",
            ));
        }
        let _guard = self.mutation_lock.lock().await;
        let Some((placement, _placement_version)) = self.store.get_placement(&report.name).await?
        else {
            return Ok(
                json!({"schema":"cosmix.nspawnd.report-ack.v1","ok":true,"accepted":false,"reason":"unplaced"}),
            );
        };
        if placement.owner != report.node || placement.generation != report.generation {
            return Ok(
                json!({"schema":"cosmix.nspawnd.report-ack.v1","ok":true,"accepted":false,"reason":"stale"}),
            );
        }
        let previous = self.store.get_observation(&report.name).await?;
        if previous
            .as_ref()
            .is_some_and(|value| value.0.reported_at > report.reported_at)
        {
            return Ok(
                json!({"schema":"cosmix.nspawnd.report-ack.v1","ok":true,"accepted":false,"reason":"stale_report"}),
            );
        }
        let expected = previous.map_or(0, |value| value.1);
        let observation = ObservationRecord {
            schema: OBSERVATION_SCHEMA.into(),
            name: report.name.clone(),
            node: report.node,
            generation: report.generation,
            state: report.state,
            image_present: report.image_present,
            unit_active: report.unit_active,
            executor_request_id: report.executor_request_id.clone(),
            executor_op_id: report.executor_op_id.clone(),
            reported_at: report.reported_at,
            received_at: now(),
        };
        let observation_version = self.store.set_observation(&observation, expected).await?;
        if let Some(claim) = placement.prepared_by.clone()
            && let Some((mut operation, operation_version)) =
                self.store.get_operation(&claim).await?
            && matches!(
                operation.phase,
                ControllerPhase::Dispatching | ControllerPhase::Unknown
            )
            && report.executor_request_id.as_deref() == Some(operation.executor_request_id.as_str())
            && report.executor_op_id.is_some()
        {
            let resolution = match report.executor_operation_state {
                Some(ExecutorTerminalState::Succeeded) => Some(ControllerPhase::Succeeded),
                Some(ExecutorTerminalState::Failed)
                    if report
                        .executor_error_code
                        .as_deref()
                        .is_some_and(outcome_unknown_error) =>
                {
                    observation_matches(operation.desired, &observation.state)
                        .then_some(ControllerPhase::Succeeded)
                }
                Some(ExecutorTerminalState::Failed) => Some(ControllerPhase::Failed),
                None => None,
            };
            if let Some(phase) = resolution {
                operation.phase = phase;
                operation.response_rc = Some(if phase == ControllerPhase::Succeeded {
                    RC_OK
                } else {
                    RC_BACKEND
                });
                operation.executor_op_id = report.executor_op_id.clone();
                operation.response_body = Some(if phase == ControllerPhase::Succeeded {
                    controller_reply(
                        &operation,
                        json!({
                            "schema":EXECUTOR_OPERATION_SCHEMA,
                            "ok":true,
                            "op_id":report.executor_op_id,
                            "outcome":"confirmed_by_report",
                            "observed":observation.state,
                        }),
                    )
                } else {
                    json!({
                        "schema":"cosmix.nspawnd.ct-error.v1",
                        "ok":false,
                        "request_id":operation.request_id,
                        "op_id":operation.op_id,
                        "error_code":"executor_rejected",
                        "retryable":false,
                        "executor":{
                            "schema":EXECUTOR_ERROR_SCHEMA,
                            "ok":false,
                            "op_id":report.executor_op_id,
                            "error_code":report.executor_error_code,
                        }
                    })
                });
                operation.completed_at = Some(now());
                self.store
                    .set_operation(&operation, operation_version)
                    .await?;
                self.clear_placement_marker(&operation).await?;
            }
        }
        Ok(
            json!({"schema":"cosmix.nspawnd.report-ack.v1","ok":true,"accepted":true,"observation_version":observation_version}),
        )
    }

    pub async fn recover(&self) -> Result<(), ApiError> {
        let _guard = self.mutation_lock.lock().await;
        for (mut operation, version) in self.store.list_operations().await? {
            operation.validate().map_err(|error| {
                ApiError::backend(
                    "state_corrupt",
                    format!("invalid recovered operation {}: {error}", operation.op_id),
                    false,
                )
            })?;
            if operation.phase.terminal() {
                self.clear_placement_marker(&operation).await?;
                continue;
            }
            match operation.phase {
                ControllerPhase::Prepared => {
                    if operation.is_adopt {
                        self.fail_recovery_operation(
                            &mut operation,
                            version,
                            "recovery_adopt_stale",
                        )
                        .await?;
                        continue;
                    }
                    let current = self.store.get_placement(&operation.name).await?;
                    if let Some((placement, _)) = &current {
                        validate_recovered_placement(placement)?;
                    }
                    let prepared = match current {
                        Some((placement, placement_version))
                            if placement_matches_operation(&placement, &operation) =>
                        {
                            Some((placement, placement_version))
                        }
                        Some((mut placement, placement_version))
                            if placement_version == operation.placement_version_before
                                && placement.op.is_none() =>
                        {
                            placement.desired = operation.desired;
                            placement.op = Some(operation.op_id.clone());
                            placement.prepared_by = Some(operation.claim_key.clone());
                            placement.intent_hash = Some(operation.request_hash.clone());
                            placement.updated_at = now();
                            let next = self
                                .store
                                .set_placement(&placement, placement_version, &operation.request_id)
                                .await?;
                            Some((placement, next))
                        }
                        None if operation.placement_version_before == 0 => {
                            let at = now();
                            let placement = PlacementRecord {
                                schema: PLACEMENT_SCHEMA.into(),
                                name: operation.name.clone(),
                                owner: operation.target.clone(),
                                generation: operation.generation,
                                grant_record_version: 1,
                                grant_record_updated: at.clone(),
                                desired: operation.desired,
                                state: "placed".into(),
                                op: Some(operation.op_id.clone()),
                                prepared_by: Some(operation.claim_key.clone()),
                                intent_hash: Some(operation.request_hash.clone()),
                                updated_at: at,
                            };
                            let next = self
                                .store
                                .set_placement(&placement, 0, &operation.request_id)
                                .await?;
                            Some((placement, next))
                        }
                        _ => None,
                    };
                    if let Some((mut placement, placement_version)) = prepared {
                        operation.phase = ControllerPhase::Ready;
                        operation.placement_version_after = Some(placement_version);
                        let next = self.store.set_operation(&operation, version).await?;
                        if placement_matches_operation(&placement, &operation) {
                            let _reply = self
                                .dispatch(&mut operation, next, &mut placement, placement_version)
                                .await?;
                        } else {
                            self.fail_recovery_operation(&mut operation, next, "recovery_conflict")
                                .await?;
                        }
                    } else {
                        self.fail_recovery_operation(
                            &mut operation,
                            version,
                            "recovery_unprepared",
                        )
                        .await?;
                    }
                }
                ControllerPhase::Ready => {
                    if let Some((mut placement, placement_version)) =
                        self.store.get_placement(&operation.name).await?
                    {
                        validate_recovered_placement(&placement)?;
                        if placement_matches_operation(&placement, &operation) {
                            let _reply = self
                                .dispatch(
                                    &mut operation,
                                    version,
                                    &mut placement,
                                    placement_version,
                                )
                                .await?;
                        } else {
                            self.fail_recovery_operation(
                                &mut operation,
                                version,
                                "recovery_conflict",
                            )
                            .await?;
                        }
                    } else {
                        self.fail_recovery_operation(&mut operation, version, "recovery_conflict")
                            .await?;
                    }
                }
                ControllerPhase::Dispatching | ControllerPhase::Unknown => {
                    // Probe the exact durable executor request key. This is a
                    // read-only resolution attempt, never a second actuation.
                    let status = self
                        .executor
                        .call(
                            &operation.target,
                            "nspawnd.request.status",
                            json!({
                                "schema":REQUEST_SCHEMA,
                                "request_id":operation.executor_request_id,
                                "name":operation.name,
                                "operation_token":self.operation_token,
                            }),
                        )
                        .await;
                    let resolution = self.recovery_probe_resolution(&operation, status).await;
                    if let Some((phase, rc, executor_body)) = resolution {
                        let retryable = reply_retryable(phase, &executor_body);
                        operation.phase = phase;
                        operation.response_rc = Some(rc);
                        operation.response_body = Some(if phase == ControllerPhase::Succeeded {
                            controller_reply(&operation, executor_body)
                        } else {
                            let executor_code =
                                executor_body.get("error_code").and_then(Value::as_str);
                            let error_code = if executor_code.is_some_and(|code| {
                                matches!(
                                    code,
                                    "never_arrived" | "did_not_take_effect" | "did_not_converge"
                                )
                            }) {
                                executor_code.unwrap()
                            } else if phase == ControllerPhase::Unknown {
                                "executor_unknown"
                            } else {
                                "executor_rejected"
                            };
                            json!({"schema":"cosmix.nspawnd.ct-error.v1","ok":false,"request_id":operation.request_id,"op_id":operation.op_id,"error_code":error_code,"retryable":retryable,"executor":executor_body})
                        });
                        operation.completed_at = phase.terminal().then(now);
                        self.store.set_operation(&operation, version).await?;
                        if phase.terminal() {
                            self.clear_placement_marker(&operation).await?;
                        }
                    } else {
                        operation.phase = ControllerPhase::Unknown;
                        operation.response_rc = Some(RC_BACKEND);
                        operation.response_body =
                            Some(json!({"ok":false,"error_code":"ambiguous_after_restart"}));
                        self.store.set_operation(&operation, version).await?;
                    }
                }
                ControllerPhase::Succeeded | ControllerPhase::Failed => {}
            }
        }
        Ok(())
    }

    async fn recovery_probe_resolution(
        &self,
        operation: &ControllerOperation,
        outcome: RemoteOutcome,
    ) -> Option<(ControllerPhase, u8, Value)> {
        let RemoteOutcome::Reply { rc: RC_OK, body } = outcome else {
            return None;
        };
        if !valid_executor_request_status_reply(&body, operation) {
            return None;
        }
        let age = operation_age(operation)?;
        if body["found"] == false {
            if body["in_flight"] == true || age < NEVER_ARRIVED_GRACE {
                return None;
            }
            if age < NEVER_ARRIVED_MAX_AGE {
                return Some((
                    ControllerPhase::Failed,
                    RC_BACKEND,
                    json!({
                        "schema":EXECUTOR_ERROR_SCHEMA,
                        "ok":false,
                        "request_id":operation.executor_request_id,
                        "name":operation.name,
                        "error_code":"never_arrived",
                        "message":"executor request was neither claimed nor in flight after the delivery grace window",
                        "retryable":false,
                    }),
                ));
            }
            return self
                .resolve_by_convergence(operation, "converged_after_horizon", "did_not_take_effect")
                .await;
        }

        let state = body.pointer("/operation/state").and_then(Value::as_str)?;
        let response_rc = body
            .pointer("/operation/response_rc")
            .and_then(Value::as_u64)
            .and_then(|value| u8::try_from(value).ok())?;
        let response_body = body.pointer("/operation/response_body").cloned()?;
        match state {
            "succeeded" => {
                let phase = classify_executor_reply(response_rc, &response_body, operation);
                Some((
                    if phase == ControllerPhase::Succeeded {
                        phase
                    } else {
                        ControllerPhase::Unknown
                    },
                    response_rc,
                    response_body,
                ))
            }
            "failed" => {
                if response_body
                    .get("error_code")
                    .and_then(Value::as_str)
                    .is_some_and(outcome_unknown_error)
                {
                    if age >= CONVERGENCE_WINDOW {
                        return self
                            .resolve_by_convergence(operation, "converged", "did_not_converge")
                            .await;
                    }
                    return Some((ControllerPhase::Unknown, response_rc, response_body));
                }
                let phase = classify_executor_reply(response_rc, &response_body, operation);
                Some((
                    if phase == ControllerPhase::Failed {
                        phase
                    } else {
                        ControllerPhase::Unknown
                    },
                    response_rc,
                    response_body,
                ))
            }
            _ => None,
        }
    }

    async fn resolve_by_convergence(
        &self,
        operation: &ControllerOperation,
        success_outcome: &'static str,
        failure_code: &'static str,
    ) -> Option<(ControllerPhase, u8, Value)> {
        let status = self
            .executor
            .call(
                &operation.target,
                "nspawnd.status",
                json!({"name":operation.name}),
            )
            .await;
        let RemoteOutcome::Reply { rc: RC_OK, body } = status else {
            return None;
        };
        if !valid_executor_status_reply(&body, &operation.name) {
            return None;
        }
        let observed = body.get("observed").and_then(Value::as_str)?;
        if observation_matches(operation.desired, observed) {
            Some((
                ControllerPhase::Succeeded,
                RC_OK,
                json!({
                    "schema":EXECUTOR_OPERATION_SCHEMA,
                    "ok":true,
                    "request_id":operation.executor_request_id,
                    "name":operation.name,
                    "op_id":operation.executor_op_id,
                    "outcome":success_outcome,
                    "observed":observed,
                }),
            ))
        } else {
            Some((
                ControllerPhase::Failed,
                RC_BACKEND,
                json!({
                    "schema":EXECUTOR_ERROR_SCHEMA,
                    "ok":false,
                    "request_id":operation.executor_request_id,
                    "name":operation.name,
                    "error_code":failure_code,
                    "message":format!("executor observation {observed:?} did not match desired {}", operation.desired.as_str()),
                    "retryable":false,
                }),
            ))
        }
    }

    async fn fail_recovery_operation(
        &self,
        operation: &mut ControllerOperation,
        operation_version: u64,
        error_code: &'static str,
    ) -> Result<(), ApiError> {
        tracing::error!(
            op_id = %operation.op_id,
            instance = %operation.name,
            error_code,
            "controller recovery refused to dispatch inconsistent durable state"
        );
        operation.phase = ControllerPhase::Failed;
        operation.response_rc = Some(RC_BACKEND);
        operation.response_body = Some(json!({
            "schema":"cosmix.nspawnd.ct-error.v1",
            "ok":false,
            "request_id":operation.request_id,
            "op_id":operation.op_id,
            "error_code":error_code,
            "message":if error_code == "recovery_adopt_stale" {
                "re-issue the adopt; preflight fencing cannot be assumed to still hold"
            } else {
                "controller recovery refused inconsistent durable state"
            },
            "retryable":false,
        }));
        operation.completed_at = Some(now());
        self.store
            .set_operation(operation, operation_version)
            .await?;
        self.clear_placement_marker_by_op(operation).await
    }

    async fn clear_placement_marker(
        &self,
        operation: &ControllerOperation,
    ) -> Result<(), ApiError> {
        if let Some((mut placement, version)) = self.store.get_placement(&operation.name).await? {
            validate_recovered_placement(&placement)?;
            if placement.prepared_by.as_deref() == Some(&operation.claim_key)
                && placement.op.as_deref() == Some(&operation.op_id)
            {
                placement.op = None;
                placement.prepared_by = None;
                placement.intent_hash = None;
                placement.updated_at = now();
                self.store
                    .set_placement(&placement, version, &operation.request_id)
                    .await?;
            }
        }
        Ok(())
    }

    async fn clear_placement_marker_by_op(
        &self,
        operation: &ControllerOperation,
    ) -> Result<(), ApiError> {
        if let Some((mut placement, version)) = self.store.get_placement(&operation.name).await? {
            validate_recovered_placement(&placement)?;
            if placement.op.as_deref() == Some(&operation.op_id) {
                placement.op = None;
                placement.prepared_by = None;
                placement.intent_hash = None;
                placement.updated_at = now();
                self.store
                    .set_placement(&placement, version, &operation.request_id)
                    .await?;
            }
        }
        Ok(())
    }
}

fn claim_key(actor: &str, request_id: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(actor.as_bytes());
    hasher.update(&[0]);
    hasher.update(request_id.as_bytes());
    hasher.finalize().to_hex().to_string()
}

fn replay_controller(operation: ControllerOperation, hash: &str) -> Result<ApiReply, ApiError> {
    if operation.request_hash != hash {
        return Err(ApiError::caller(
            "request_conflict",
            "request_id was reused with different intent",
        ));
    }
    if operation.phase.terminal() {
        let rc = operation.response_rc.unwrap_or(RC_BACKEND);
        let body = operation
            .response_body
            .unwrap_or_else(|| json!({"ok":false,"error_code":"missing_result"}));
        return Ok(ApiReply { rc, body });
    }
    let mut error = ApiError::backend(
        "operation_in_progress",
        format!("operation {} is {:?}", operation.op_id, operation.phase),
        true,
    );
    error.op_id = Some(operation.op_id);
    Err(error)
}

fn classify_remote(
    operation: &ControllerOperation,
    outcome: RemoteOutcome,
) -> (ControllerPhase, u8, Value) {
    match outcome {
        RemoteOutcome::Reply { rc, body } => {
            let phase = classify_executor_reply(rc, &body, operation);
            let controller_rc = if rc == RC_OK && phase != ControllerPhase::Succeeded {
                RC_BACKEND
            } else {
                rc
            };
            (phase, controller_rc, body)
        }
        RemoteOutcome::RejectedBeforeSend(message) => (
            ControllerPhase::Failed,
            RC_BACKEND,
            json!({"ok":false,"error_code":"not_sent","message":message}),
        ),
        RemoteOutcome::Ambiguous(message) => (
            ControllerPhase::Unknown,
            RC_BACKEND,
            json!({"ok":false,"error_code":"ambiguous","message":message}),
        ),
    }
}

fn classify_executor_reply(
    rc: u8,
    body: &Value,
    operation: &ControllerOperation,
) -> ControllerPhase {
    if rc == RC_OK && valid_executor_operation_reply(body, operation) {
        ControllerPhase::Succeeded
    } else if rc != RC_OK
        && is_executor_error_for(body, Some(&operation.executor_request_id), &operation.name)
    {
        ControllerPhase::Failed
    } else {
        ControllerPhase::Unknown
    }
}

fn is_executor_error_for(body: &Value, request_id: Option<&str>, name: &InstanceName) -> bool {
    body.get("schema").and_then(Value::as_str) == Some(EXECUTOR_ERROR_SCHEMA)
        && body.get("ok").and_then(Value::as_bool) == Some(false)
        && body.get("error_code").and_then(Value::as_str).is_some()
        && body.get("name").and_then(Value::as_str) == Some(name.as_str())
        && request_id
            .is_none_or(|expected| body.get("request_id").and_then(Value::as_str) == Some(expected))
}

fn valid_executor_operation_reply(body: &Value, operation: &ControllerOperation) -> bool {
    body.get("schema").and_then(Value::as_str) == Some(EXECUTOR_OPERATION_SCHEMA)
        && body.get("ok").and_then(Value::as_bool) == Some(true)
        && body.get("request_id").and_then(Value::as_str)
            == Some(operation.executor_request_id.as_str())
        && body.get("name").and_then(Value::as_str) == Some(operation.name.as_str())
        && body.get("op_id").and_then(Value::as_str).is_some()
        && body.get("outcome").and_then(Value::as_str).is_some()
        && body.get("observed").and_then(Value::as_str).is_some()
}

fn valid_executor_status_reply(body: &Value, name: &InstanceName) -> bool {
    body.get("schema").and_then(Value::as_str) == Some(EXECUTOR_STATUS_SCHEMA)
        && body.get("ok").and_then(Value::as_bool) == Some(true)
        && body.get("name").and_then(Value::as_str) == Some(name.as_str())
        && body.get("managed").and_then(Value::as_bool).is_some()
        && body
            .get("observed")
            .and_then(Value::as_str)
            .is_some_and(|state| matches!(state, "running" | "stopped" | "absent"))
        && body.get("current_operation").is_some()
        && body
            .get("grant_generation")
            .is_some_and(|value| value.is_null() || value.as_u64().is_some())
}

fn valid_executor_request_status_reply(body: &Value, operation: &ControllerOperation) -> bool {
    body.get("schema").and_then(Value::as_str) == Some(EXECUTOR_REQUEST_STATUS_SCHEMA)
        && body.get("ok").and_then(Value::as_bool) == Some(true)
        && body.get("request_id").and_then(Value::as_str)
            == Some(operation.executor_request_id.as_str())
        && body.get("name").and_then(Value::as_str) == Some(operation.name.as_str())
        && body.get("found").and_then(Value::as_bool).is_some()
        && body.get("in_flight").and_then(Value::as_bool).is_some()
        && body.get("operation").is_some()
        && (body["found"] == false
            || (body
                .pointer("/operation/request_id")
                .and_then(Value::as_str)
                == Some(operation.executor_request_id.as_str())
                && body.pointer("/operation/name").and_then(Value::as_str)
                    == Some(operation.name.as_str())))
}

fn operation_age(operation: &ControllerOperation) -> Option<Duration> {
    DateTime::parse_from_rfc3339(&operation.started_at)
        .ok()
        .and_then(|started| {
            Utc::now()
                .signed_duration_since(started.with_timezone(&Utc))
                .to_std()
                .ok()
        })
}

fn outcome_unknown_error(code: &str) -> bool {
    matches!(code, "interrupted" | "timeout" | "outcome_unknown")
}

fn placement_matches_operation(
    placement: &PlacementRecord,
    operation: &ControllerOperation,
) -> bool {
    placement.op.as_deref() == Some(operation.op_id.as_str())
        && placement.prepared_by.as_deref() == Some(operation.claim_key.as_str())
        && placement.intent_hash.as_deref() == Some(operation.request_hash.as_str())
        && placement.desired == operation.desired
        && placement.owner == operation.target
        && placement.generation == operation.generation
}

fn validate_recovered_placement(placement: &PlacementRecord) -> Result<(), ApiError> {
    placement.validate().map_err(|error| {
        ApiError::backend(
            "state_corrupt",
            format!("invalid recovered placement {}: {error}", placement.name),
            false,
        )
    })
}

fn reply_retryable(phase: ControllerPhase, body: &Value) -> bool {
    phase == ControllerPhase::Unknown
        || body
            .get("retryable")
            .and_then(Value::as_bool)
            .unwrap_or(false)
}

fn controller_reply(operation: &ControllerOperation, executor: Value) -> Value {
    json!({
        "schema":"cosmix.nspawnd.ct-operation.v1","ok":true,"request_id":operation.request_id,
        "op_id":operation.op_id,"name":operation.name,"owner":operation.target,
        "generation":operation.generation,"desired":operation.desired,"executor":executor
    })
}

fn observation_matches(desired: DesiredState, observed: &str) -> bool {
    matches!(
        (desired, observed),
        (DesiredState::Running, "running") | (DesiredState::Stopped, "stopped" | "absent")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::RC_CALLER;
    use std::collections::BTreeMap;

    struct StaticClient(RemoteOutcome);

    #[async_trait]
    impl ExecutorClient for StaticClient {
        async fn call(&self, _node: &str, _verb: &str, request: Value) -> RemoteOutcome {
            let mut outcome = self.0.clone();
            if let RemoteOutcome::Reply { body, .. } = &mut outcome {
                if request.get("request_id").is_some() {
                    body["request_id"] = request["request_id"].clone();
                }
                if request.get("name").is_some() {
                    body["name"] = request["name"].clone();
                }
            }
            outcome
        }
    }

    struct AdoptClient {
        statuses: BTreeMap<String, RemoteOutcome>,
        mutation: RemoteOutcome,
        calls: Arc<std::sync::Mutex<Vec<(String, String)>>>,
    }

    struct VerbClient {
        outcomes: std::sync::Mutex<BTreeMap<String, Vec<RemoteOutcome>>>,
    }

    #[async_trait]
    impl ExecutorClient for VerbClient {
        async fn call(&self, _node: &str, verb: &str, _request: Value) -> RemoteOutcome {
            self.outcomes
                .lock()
                .unwrap()
                .get_mut(verb)
                .and_then(|outcomes| (!outcomes.is_empty()).then(|| outcomes.remove(0)))
                .unwrap_or_else(|| RemoteOutcome::Ambiguous(format!("no fake reply for {verb}")))
        }
    }

    #[async_trait]
    impl ExecutorClient for AdoptClient {
        async fn call(&self, node: &str, verb: &str, request: Value) -> RemoteOutcome {
            self.calls
                .lock()
                .unwrap()
                .push((node.to_owned(), verb.to_owned()));
            if verb == "nspawnd.status" {
                self.statuses
                    .get(node)
                    .cloned()
                    .unwrap_or_else(|| RemoteOutcome::Ambiguous("missing fake status".into()))
            } else {
                let mut outcome = self.mutation.clone();
                if let RemoteOutcome::Reply { body, .. } = &mut outcome {
                    body["request_id"] = request["request_id"].clone();
                    body["name"] = request["name"].clone();
                }
                outcome
            }
        }
    }

    fn status_reply(managed: bool, observed: &str, generation: Option<u64>) -> RemoteOutcome {
        RemoteOutcome::Reply {
            rc: RC_OK,
            body: json!({
                "schema":EXECUTOR_STATUS_SCHEMA,
                "ok":true,
                "name":"demo",
                "managed":managed,
                "observed":observed,
                "current_operation":null,
                "grant_generation":generation,
            }),
        }
    }

    fn operation_reply(outcome: &str, observed: &str) -> RemoteOutcome {
        RemoteOutcome::Reply {
            rc: RC_OK,
            body: json!({
                "schema":EXECUTOR_OPERATION_SCHEMA,
                "ok":true,
                "request_id":"ct-recovery-op",
                "name":"demo",
                "op_id":"executor-op",
                "outcome":outcome,
                "observed":observed,
            }),
        }
    }

    fn started_at_ago(seconds: i64) -> String {
        (Utc::now() - chrono::Duration::seconds(seconds)).to_rfc3339_opts(SecondsFormat::Secs, true)
    }

    fn name() -> InstanceName {
        InstanceName::parse("demo").unwrap()
    }

    fn marked_placement() -> PlacementRecord {
        PlacementRecord {
            schema: PLACEMENT_SCHEMA.into(),
            name: name(),
            owner: "alpha".into(),
            generation: 2,
            grant_record_version: 1,
            grant_record_updated: "2026-08-09T00:00:00Z".into(),
            desired: DesiredState::Stopped,
            state: "placed".into(),
            op: Some("controller-op".into()),
            prepared_by: Some("claim-recovery".into()),
            intent_hash: Some("intent-recovery".into()),
            updated_at: "2026-08-09T00:00:00Z".into(),
        }
    }

    fn recovery_operation(phase: ControllerPhase) -> ControllerOperation {
        let terminal = phase.terminal();
        ControllerOperation {
            schema: CONTROLLER_OPERATION_SCHEMA.into(),
            claim_key: "claim-recovery".into(),
            op_id: "controller-op".into(),
            actor: "operator".into(),
            request_id: "recover-1".into(),
            request_hash: "intent-recovery".into(),
            is_adopt: false,
            verb: ControllerVerb::Stop,
            name: name(),
            target: "alpha".into(),
            generation: 2,
            desired: DesiredState::Stopped,
            executor_request_id: "ct-controller-op".into(),
            phase,
            placement_version_before: 1,
            placement_version_after: Some(2),
            executor_op_id: None,
            response_rc: terminal.then_some(RC_OK),
            response_body: terminal.then(|| json!({"ok":true})),
            started_at: "2026-08-09T00:00:00Z".into(),
            completed_at: terminal.then(|| "2026-08-09T00:00:01Z".into()),
        }
    }

    fn start_request(request_id: &str) -> ControllerMutationRequest {
        ControllerMutationRequest {
            schema: CONTROLLER_REQUEST_SCHEMA.into(),
            name: name(),
            owner: None,
            generation: None,
            if_version: 1,
            request_id: request_id.into(),
            operation_token: "operator-token".into(),
        }
    }

    fn executor_report(
        terminal: Option<ExecutorTerminalState>,
        error_code: Option<&str>,
        state: &str,
    ) -> ExecutorReport {
        ExecutorReport {
            schema: REPORT_SCHEMA.into(),
            name: name(),
            node: "alpha".into(),
            generation: 2,
            state: state.into(),
            image_present: true,
            unit_active: if state == "running" {
                "active".into()
            } else {
                "inactive".into()
            },
            executor_request_id: Some("ct-controller-op".into()),
            executor_op_id: Some("executor-op".into()),
            executor_operation_state: terminal,
            executor_error_code: error_code.map(str::to_owned),
            reported_at: "2026-08-09T00:00:02Z".into(),
            operation_token: "shared-token".into(),
        }
    }

    #[test]
    fn placement_observation_and_operation_validation_is_strict() {
        let placement = PlacementRecord {
            schema: PLACEMENT_SCHEMA.into(),
            name: name(),
            owner: "alpha".into(),
            generation: 2,
            grant_record_version: 1,
            grant_record_updated: "2026-08-09T00:00:00Z".into(),
            desired: DesiredState::Stopped,
            state: "placed".into(),
            op: None,
            prepared_by: None,
            intent_hash: None,
            updated_at: "2026-08-09T00:00:00Z".into(),
        };
        assert!(placement.validate().is_ok());
        let mut bad = placement.clone();
        bad.generation = 0;
        assert!(bad.validate().is_err());
        assert!(serde_json::from_value::<PlacementRecord>(json!({"extra":1})).is_err());

        let observation = ObservationRecord {
            schema: OBSERVATION_SCHEMA.into(),
            name: name(),
            node: "alpha".into(),
            generation: 2,
            state: "stopped".into(),
            image_present: true,
            unit_active: "inactive".into(),
            executor_request_id: None,
            executor_op_id: None,
            reported_at: "2026-08-09T00:00:00Z".into(),
            received_at: "2026-08-09T00:00:01Z".into(),
        };
        assert!(observation.validate().is_ok());

        let operation = ControllerOperation {
            schema: CONTROLLER_OPERATION_SCHEMA.into(),
            claim_key: "claim".into(),
            op_id: "op".into(),
            actor: "operator".into(),
            request_id: "req-1".into(),
            request_hash: "hash".into(),
            is_adopt: false,
            verb: ControllerVerb::Start,
            name: name(),
            target: "alpha".into(),
            generation: 2,
            desired: DesiredState::Running,
            executor_request_id: "ct-op".into(),
            phase: ControllerPhase::Prepared,
            placement_version_before: 1,
            placement_version_after: None,
            executor_op_id: None,
            response_rc: None,
            response_body: None,
            started_at: "2026-08-09T00:00:00Z".into(),
            completed_at: None,
        };
        assert!(operation.validate().is_ok());
        let mut old_json = serde_json::to_value(&operation).unwrap();
        old_json.as_object_mut().unwrap().remove("is_adopt");
        assert!(
            !serde_json::from_value::<ControllerOperation>(old_json)
                .unwrap()
                .is_adopt
        );
        let mut bad_operation = operation;
        bad_operation.phase = ControllerPhase::Succeeded;
        assert!(bad_operation.validate().is_err());
    }

    #[tokio::test]
    async fn props_cas_requires_the_current_version() {
        let temp = tempfile::tempdir().unwrap();
        let store = ControllerStore::open(&temp.path().join("controller.db")).unwrap();
        let placement = PlacementRecord {
            schema: PLACEMENT_SCHEMA.into(),
            name: name(),
            owner: "alpha".into(),
            generation: 1,
            grant_record_version: 1,
            grant_record_updated: "2026-08-09T00:00:00Z".into(),
            desired: DesiredState::Stopped,
            state: "placed".into(),
            op: None,
            prepared_by: None,
            intent_hash: None,
            updated_at: "2026-08-09T00:00:00Z".into(),
        };
        assert_eq!(store.set_placement(&placement, 0, "test").await.unwrap(), 1);
        assert_eq!(store.get_placement(&name()).await.unwrap().unwrap().1, 1);
        assert!(store.set_placement(&placement, 0, "stale").await.is_err());
        let peer = cosmix_props::namespace::PeerIdentity {
            service_name: Some("agent".into()),
            ..Default::default()
        };
        let caps = store.instances.spec().auth.resolve(&peer);
        assert!(caps.contains(&Capability::new("props.read:nspawnd.instances")));
        assert!(caps.contains(&Capability::new("props.describe:nspawnd.instances:public")));
        assert!(!caps.contains(&Capability::new("props.describe:nspawnd.instances")));
        assert!(!caps.contains(&Capability::new("props.write:nspawnd.instances")));
    }

    #[tokio::test]
    async fn controller_claims_cas_dispatches_and_replays_exact_result() {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(ControllerStore::open(&temp.path().join("controller.db")).unwrap());
        let client = Arc::new(AdoptClient {
            statuses: BTreeMap::from([("alpha".into(), status_reply(true, "stopped", Some(1)))]),
            mutation: operation_reply("already_stopped", "stopped"),
            calls: Arc::new(std::sync::Mutex::new(Vec::new())),
        });
        let service = ControllerService::new(
            store.clone(),
            client,
            "shared-token".into(),
            ["alpha".into()],
        );
        let request = ControllerMutationRequest {
            schema: CONTROLLER_REQUEST_SCHEMA.into(),
            name: name(),
            owner: Some("alpha".into()),
            generation: Some(2),
            if_version: 0,
            request_id: "adopt-1".into(),
            operation_token: "operator-token".into(),
        };
        let first = service
            .mutate("operator", request.clone(), ControllerVerb::Adopt)
            .await
            .unwrap();
        let replay = service
            .mutate("operator", request, ControllerVerb::Adopt)
            .await
            .unwrap();
        assert_eq!(first.body, replay.body);
        assert_eq!(first.rc, RC_OK);
        let (placement, version) = store.get_placement(&name()).await.unwrap().unwrap();
        assert_eq!(version, 2);
        assert_eq!(placement.op, None);
        let operations = store.list_operations().await.unwrap();
        assert_eq!(operations.len(), 1);
        assert_eq!(operations[0].0.phase, ControllerPhase::Succeeded);
    }

    #[tokio::test]
    async fn schema_less_mesh_error_keeps_marker_but_executor_error_clears_it() {
        for (request_id, body, expected_phase, marker_retained) in [
            (
                "mesh-timeout",
                json!({"ok":false,"message":"Mesh bridge error: timeout"}),
                ControllerPhase::Unknown,
                true,
            ),
            (
                "executor-refusal",
                json!({
                    "schema":EXECUTOR_ERROR_SCHEMA,
                    "ok":false,
                    "error_code":"state_conflict",
                    "retryable":false,
                }),
                ControllerPhase::Failed,
                false,
            ),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let store =
                Arc::new(ControllerStore::open(&temp.path().join("controller.db")).unwrap());
            let mut placement = marked_placement();
            placement.op = None;
            placement.prepared_by = None;
            placement.intent_hash = None;
            store.set_placement(&placement, 0, "seed").await.unwrap();
            let service = ControllerService::new(
                store.clone(),
                Arc::new(StaticClient(RemoteOutcome::Reply {
                    rc: RC_CALLER,
                    body,
                })),
                "shared-token".into(),
                ["alpha".into()],
            );
            service
                .mutate("operator", start_request(request_id), ControllerVerb::Start)
                .await
                .unwrap();
            let operation = store.list_operations().await.unwrap().remove(0).0;
            assert_eq!(operation.phase, expected_phase);
            assert_eq!(
                store
                    .get_placement(&name())
                    .await
                    .unwrap()
                    .unwrap()
                    .0
                    .op
                    .is_some(),
                marker_retained
            );
        }
    }

    #[tokio::test]
    async fn adopt_fences_every_reporter_listed_executor_and_rejects_conflict() {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(ControllerStore::open(&temp.path().join("controller.db")).unwrap());
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let client = Arc::new(AdoptClient {
            statuses: BTreeMap::from([
                ("alpha".into(), status_reply(true, "running", Some(1))),
                ("beta".into(), status_reply(true, "stopped", Some(1))),
            ]),
            mutation: operation_reply("already_stopped", "stopped"),
            calls: calls.clone(),
        });
        let service = ControllerService::new(
            store,
            client,
            "shared-token".into(),
            ["alpha".into(), "beta".into()],
        );
        let error = service
            .mutate(
                "operator",
                ControllerMutationRequest {
                    schema: CONTROLLER_REQUEST_SCHEMA.into(),
                    name: name(),
                    owner: Some("beta".into()),
                    generation: Some(2),
                    if_version: 0,
                    request_id: "adopt-conflict".into(),
                    operation_token: "operator-token".into(),
                },
                ControllerVerb::Adopt,
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, "conflicting_owner");
        let calls = calls.lock().unwrap();
        assert!(calls.contains(&("alpha".into(), "nspawnd.status".into())));
        assert!(calls.contains(&("beta".into(), "nspawnd.status".into())));
    }

    #[tokio::test]
    async fn adopt_generation_must_exceed_every_executor_grant() {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(ControllerStore::open(&temp.path().join("controller.db")).unwrap());
        let client = Arc::new(AdoptClient {
            statuses: BTreeMap::from([
                ("alpha".into(), status_reply(false, "absent", Some(2))),
                ("beta".into(), status_reply(true, "stopped", Some(1))),
            ]),
            mutation: operation_reply("already_stopped", "stopped"),
            calls: Arc::new(std::sync::Mutex::new(Vec::new())),
        });
        let service = ControllerService::new(
            store,
            client,
            "shared-token".into(),
            ["alpha".into(), "beta".into()],
        );
        assert_eq!(
            service
                .adopt_preflight("beta", &name(), 2)
                .await
                .unwrap_err()
                .code,
            "generation_stale"
        );
    }

    #[tokio::test]
    async fn adopt_rejects_unmanaged_running_copy_on_another_executor() {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(ControllerStore::open(&temp.path().join("controller.db")).unwrap());
        let service = ControllerService::new(
            store,
            Arc::new(AdoptClient {
                statuses: BTreeMap::from([
                    ("alpha".into(), status_reply(false, "running", None)),
                    ("beta".into(), status_reply(true, "stopped", Some(1))),
                ]),
                mutation: operation_reply("already_stopped", "stopped"),
                calls: Arc::new(std::sync::Mutex::new(Vec::new())),
            }),
            "shared-token".into(),
            ["alpha".into(), "beta".into()],
        );
        assert_eq!(
            service
                .adopt_preflight("beta", &name(), 2)
                .await
                .unwrap_err()
                .code,
            "conflicting_running"
        );
    }

    #[tokio::test]
    async fn adopt_refuses_without_roster_or_with_untrusted_status_error() {
        let request = || ControllerMutationRequest {
            schema: CONTROLLER_REQUEST_SCHEMA.into(),
            name: name(),
            owner: Some("alpha".into()),
            generation: Some(2),
            if_version: 0,
            request_id: "adopt-unsafe".into(),
            operation_token: "operator-token".into(),
        };
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(ControllerStore::open(&temp.path().join("empty.db")).unwrap());
        let service = ControllerService::new(
            store,
            Arc::new(StaticClient(RemoteOutcome::Ambiguous("unused".into()))),
            "shared-token".into(),
            Vec::<String>::new(),
        );
        assert_eq!(
            service
                .mutate("operator", request(), ControllerVerb::Adopt)
                .await
                .unwrap_err()
                .code,
            "adopt_fencing_unavailable"
        );

        let store = Arc::new(ControllerStore::open(&temp.path().join("untrusted.db")).unwrap());
        let service = ControllerService::new(
            store,
            Arc::new(StaticClient(RemoteOutcome::Reply {
                rc: RC_CALLER,
                body: json!({"ok":false,"message":"Mesh bridge error: timeout"}),
            })),
            "shared-token".into(),
            ["alpha".into()],
        );
        let error = service
            .mutate("operator", request(), ControllerVerb::Adopt)
            .await
            .unwrap_err();
        assert_eq!(error.code, "executor_status_unknown");
        assert!(error.retryable);
    }

    #[tokio::test]
    async fn recovery_finishes_the_missing_placement_cas_before_dispatch() {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(ControllerStore::open(&temp.path().join("controller.db")).unwrap());
        let operation = ControllerOperation {
            schema: CONTROLLER_OPERATION_SCHEMA.into(),
            claim_key: "claim-recovery".into(),
            op_id: "recovery-op".into(),
            actor: "operator".into(),
            request_id: "recover-1".into(),
            request_hash: "intent-recovery".into(),
            is_adopt: false,
            verb: ControllerVerb::Adopt,
            name: name(),
            target: "alpha".into(),
            generation: 2,
            desired: DesiredState::Stopped,
            executor_request_id: "ct-recovery-op".into(),
            phase: ControllerPhase::Prepared,
            placement_version_before: 0,
            placement_version_after: None,
            executor_op_id: None,
            response_rc: None,
            response_body: None,
            started_at: "2026-08-09T00:00:00Z".into(),
            completed_at: None,
        };
        store.set_operation(&operation, 0).await.unwrap();
        let client = Arc::new(StaticClient(operation_reply("already_stopped", "stopped")));
        let service = ControllerService::new(
            store.clone(),
            client,
            "shared-token".into(),
            ["alpha".into()],
        );
        service.recover().await.unwrap();
        let (placement, version) = store.get_placement(&name()).await.unwrap().unwrap();
        assert_eq!(version, 2);
        assert!(placement.prepared_by.is_none());
        let (recovered, _) = store
            .get_operation("claim-recovery")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(recovered.phase, ControllerPhase::Succeeded);
    }

    #[tokio::test]
    async fn recovery_refuses_to_reuse_adopt_preflight() {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(ControllerStore::open(&temp.path().join("controller.db")).unwrap());
        store
            .set_placement(&marked_placement(), 0, "prepare")
            .await
            .unwrap();
        let mut operation = recovery_operation(ControllerPhase::Prepared);
        operation.is_adopt = true;
        operation.verb = ControllerVerb::Adopt;
        store.set_operation(&operation, 0).await.unwrap();
        let service = ControllerService::new(
            store.clone(),
            Arc::new(StaticClient(RemoteOutcome::Ambiguous(
                "must not probe or dispatch".into(),
            ))),
            "shared-token".into(),
            ["alpha".into()],
        );
        service.recover().await.unwrap();
        let operation = store
            .get_operation("claim-recovery")
            .await
            .unwrap()
            .unwrap()
            .0;
        assert_eq!(operation.phase, ControllerPhase::Failed);
        assert_eq!(
            operation.response_body.unwrap()["error_code"],
            "recovery_adopt_stale"
        );
        assert!(
            store
                .get_placement(&name())
                .await
                .unwrap()
                .unwrap()
                .0
                .op
                .is_none()
        );
    }

    #[tokio::test]
    async fn recovery_cleans_terminal_placement_marker() {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(ControllerStore::open(&temp.path().join("controller.db")).unwrap());
        store
            .set_placement(&marked_placement(), 0, "prepare")
            .await
            .unwrap();
        store
            .set_operation(&recovery_operation(ControllerPhase::Succeeded), 0)
            .await
            .unwrap();
        let service = ControllerService::new(
            store.clone(),
            Arc::new(StaticClient(RemoteOutcome::Ambiguous("unused".into()))),
            "shared-token".into(),
            ["alpha".into()],
        );
        service.recover().await.unwrap();
        let (placement, version) = store.get_placement(&name()).await.unwrap().unwrap();
        assert_eq!(version, 2);
        assert!(placement.op.is_none());
        assert!(placement.prepared_by.is_none());
    }

    #[tokio::test]
    async fn recovery_keeps_interrupted_executor_outcome_unknown() {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(ControllerStore::open(&temp.path().join("controller.db")).unwrap());
        store
            .set_placement(&marked_placement(), 0, "prepare")
            .await
            .unwrap();
        store
            .set_operation(
                &{
                    let mut operation = recovery_operation(ControllerPhase::Unknown);
                    operation.started_at = started_at_ago(5 * 60);
                    operation
                },
                0,
            )
            .await
            .unwrap();
        let client = Arc::new(StaticClient(RemoteOutcome::Reply {
            rc: RC_OK,
            body: json!({
                "schema":EXECUTOR_REQUEST_STATUS_SCHEMA,
                "ok":true,
                "request_id":"ct-controller-op",
                "name":"demo",
                "found":true,
                "in_flight":false,
                "operation":{
                    "request_id":"ct-controller-op",
                    "name":"demo",
                    "state":"failed",
                    "response_rc":RC_BACKEND,
                    "response_body":{"schema":EXECUTOR_ERROR_SCHEMA,"ok":false,"request_id":"ct-controller-op","name":"demo","error_code":"interrupted","retryable":true}
                }
            }),
        }));
        let service = ControllerService::new(
            store.clone(),
            client,
            "shared-token".into(),
            ["alpha".into()],
        );
        service.recover().await.unwrap();
        let (operation, _) = store
            .get_operation("claim-recovery")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(operation.phase, ControllerPhase::Unknown);
        assert!(operation.completed_at.is_none());
        assert!(
            store
                .get_placement(&name())
                .await
                .unwrap()
                .unwrap()
                .0
                .op
                .is_some()
        );
    }

    #[tokio::test]
    async fn recovery_terminalises_definitive_failure_and_never_arrived() {
        for (body, expected_code) in [
            (
                json!({
                    "schema":EXECUTOR_REQUEST_STATUS_SCHEMA,
                    "ok":true,
                    "request_id":"ct-controller-op",
                    "name":"demo",
                    "found":true,
                    "in_flight":false,
                    "operation":{
                        "request_id":"ct-controller-op",
                        "name":"demo",
                        "state":"failed",
                        "response_rc":RC_CALLER,
                        "response_body":{
                            "schema":EXECUTOR_ERROR_SCHEMA,
                            "ok":false,
                            "request_id":"ct-controller-op",
                            "name":"demo",
                            "error_code":"state_conflict",
                            "retryable":false,
                        }
                    }
                }),
                "executor_rejected",
            ),
            (
                json!({
                    "schema":EXECUTOR_REQUEST_STATUS_SCHEMA,
                    "ok":true,
                    "request_id":"ct-controller-op",
                    "name":"demo",
                    "found":false,
                    "in_flight":false,
                    "operation":null,
                }),
                "never_arrived",
            ),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let store =
                Arc::new(ControllerStore::open(&temp.path().join("controller.db")).unwrap());
            store
                .set_placement(&marked_placement(), 0, "prepare")
                .await
                .unwrap();
            store
                .set_operation(
                    &{
                        let mut operation = recovery_operation(ControllerPhase::Unknown);
                        operation.started_at = started_at_ago(5 * 60);
                        operation
                    },
                    0,
                )
                .await
                .unwrap();
            let service = ControllerService::new(
                store.clone(),
                Arc::new(StaticClient(RemoteOutcome::Reply { rc: RC_OK, body })),
                "shared-token".into(),
                ["alpha".into()],
            );
            service.recover().await.unwrap();
            let (operation, _) = store
                .get_operation("claim-recovery")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(operation.phase, ControllerPhase::Failed);
            assert_eq!(
                operation.response_body.unwrap()["error_code"],
                expected_code
            );
            assert!(
                store
                    .get_placement(&name())
                    .await
                    .unwrap()
                    .unwrap()
                    .0
                    .op
                    .is_none()
            );
        }
    }

    #[tokio::test]
    async fn recovery_keeps_in_flight_unclaimed_request_unknown() {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(ControllerStore::open(&temp.path().join("controller.db")).unwrap());
        store
            .set_placement(&marked_placement(), 0, "prepare")
            .await
            .unwrap();
        store
            .set_operation(
                &{
                    let mut operation = recovery_operation(ControllerPhase::Unknown);
                    operation.started_at = started_at_ago(5 * 60);
                    operation
                },
                0,
            )
            .await
            .unwrap();
        let service = ControllerService::new(
            store.clone(),
            Arc::new(StaticClient(RemoteOutcome::Reply {
                rc: RC_OK,
                body: json!({
                    "schema":EXECUTOR_REQUEST_STATUS_SCHEMA,
                    "ok":true,
                    "request_id":"ct-controller-op",
                    "name":"demo",
                    "found":false,
                    "in_flight":true,
                    "operation":null,
                }),
            })),
            "shared-token".into(),
            ["alpha".into()],
        );
        service.recover().await.unwrap();
        assert_eq!(
            store
                .get_operation("claim-recovery")
                .await
                .unwrap()
                .unwrap()
                .0
                .phase,
            ControllerPhase::Unknown
        );
        assert!(
            store
                .get_placement(&name())
                .await
                .unwrap()
                .unwrap()
                .0
                .op
                .is_some()
        );
    }

    #[tokio::test]
    async fn evicted_request_resolves_only_from_current_convergence() {
        for (status, expected_phase, expected_code) in [
            (
                status_reply(true, "stopped", Some(2)),
                ControllerPhase::Succeeded,
                "converged_after_horizon",
            ),
            (
                status_reply(true, "running", Some(2)),
                ControllerPhase::Failed,
                "did_not_take_effect",
            ),
            (
                RemoteOutcome::Ambiguous("executor offline".into()),
                ControllerPhase::Unknown,
                "ambiguous_after_restart",
            ),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let store =
                Arc::new(ControllerStore::open(&temp.path().join("controller.db")).unwrap());
            store
                .set_placement(&marked_placement(), 0, "prepare")
                .await
                .unwrap();
            let mut operation = recovery_operation(ControllerPhase::Unknown);
            operation.started_at = started_at_ago(2 * 60 * 60);
            store.set_operation(&operation, 0).await.unwrap();
            let request_status = RemoteOutcome::Reply {
                rc: RC_OK,
                body: json!({
                    "schema":EXECUTOR_REQUEST_STATUS_SCHEMA,
                    "ok":true,
                    "request_id":"ct-controller-op",
                    "name":"demo",
                    "found":false,
                    "in_flight":false,
                    "operation":null,
                }),
            };
            let service = ControllerService::new(
                store.clone(),
                Arc::new(VerbClient {
                    outcomes: std::sync::Mutex::new(BTreeMap::from([
                        ("nspawnd.request.status".into(), vec![request_status]),
                        ("nspawnd.status".into(), vec![status]),
                    ])),
                }),
                "shared-token".into(),
                ["alpha".into()],
            );
            service.recover().await.unwrap();
            let operation = store
                .get_operation("claim-recovery")
                .await
                .unwrap()
                .unwrap()
                .0;
            assert_eq!(operation.phase, expected_phase);
            let body = operation.response_body.unwrap();
            if expected_phase == ControllerPhase::Succeeded {
                assert_eq!(body["executor"]["outcome"], expected_code);
            } else {
                assert_eq!(body["error_code"], expected_code);
            }
            assert_eq!(
                store
                    .get_placement(&name())
                    .await
                    .unwrap()
                    .unwrap()
                    .0
                    .op
                    .is_some(),
                expected_phase == ControllerPhase::Unknown
            );
        }
    }

    #[tokio::test]
    async fn old_outcome_unknown_closes_by_convergence() {
        for (status, expected_phase, expected_code) in [
            (
                status_reply(true, "stopped", Some(2)),
                ControllerPhase::Succeeded,
                "converged",
            ),
            (
                status_reply(true, "running", Some(2)),
                ControllerPhase::Failed,
                "did_not_converge",
            ),
            (
                RemoteOutcome::Ambiguous("executor offline".into()),
                ControllerPhase::Unknown,
                "ambiguous_after_restart",
            ),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let store =
                Arc::new(ControllerStore::open(&temp.path().join("controller.db")).unwrap());
            store
                .set_placement(&marked_placement(), 0, "prepare")
                .await
                .unwrap();
            let mut operation = recovery_operation(ControllerPhase::Unknown);
            operation.started_at = started_at_ago(16 * 60);
            store.set_operation(&operation, 0).await.unwrap();
            let request_status = RemoteOutcome::Reply {
                rc: RC_OK,
                body: json!({
                    "schema":EXECUTOR_REQUEST_STATUS_SCHEMA,
                    "ok":true,
                    "request_id":"ct-controller-op",
                    "name":"demo",
                    "found":true,
                    "in_flight":false,
                    "operation":{
                        "request_id":"ct-controller-op",
                        "name":"demo",
                        "state":"failed",
                        "response_rc":RC_BACKEND,
                        "response_body":{
                            "schema":EXECUTOR_ERROR_SCHEMA,
                            "ok":false,
                            "request_id":"ct-controller-op",
                            "name":"demo",
                            "error_code":"outcome_unknown",
                            "retryable":true,
                        }
                    }
                }),
            };
            let service = ControllerService::new(
                store.clone(),
                Arc::new(VerbClient {
                    outcomes: std::sync::Mutex::new(BTreeMap::from([
                        ("nspawnd.request.status".into(), vec![request_status]),
                        ("nspawnd.status".into(), vec![status]),
                    ])),
                }),
                "shared-token".into(),
                ["alpha".into()],
            );
            service.recover().await.unwrap();
            let operation = store
                .get_operation("claim-recovery")
                .await
                .unwrap()
                .unwrap()
                .0;
            assert_eq!(operation.phase, expected_phase);
            let body = operation.response_body.unwrap();
            if expected_phase == ControllerPhase::Succeeded {
                assert_eq!(body["executor"]["outcome"], expected_code);
            } else {
                assert_eq!(body["error_code"], expected_code);
            }
            assert_eq!(
                store
                    .get_placement(&name())
                    .await
                    .unwrap()
                    .unwrap()
                    .0
                    .op
                    .is_some(),
                expected_phase == ControllerPhase::Unknown
            );
        }
    }

    #[tokio::test]
    async fn ready_recovery_conflict_never_dispatches_and_only_clears_its_marker() {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(ControllerStore::open(&temp.path().join("controller.db")).unwrap());
        let mut placement = marked_placement();
        placement.intent_hash = Some("different-intent".into());
        store.set_placement(&placement, 0, "prepare").await.unwrap();
        store
            .set_operation(&recovery_operation(ControllerPhase::Ready), 0)
            .await
            .unwrap();
        let service = ControllerService::new(
            store.clone(),
            Arc::new(StaticClient(RemoteOutcome::Ambiguous(
                "must not dispatch".into(),
            ))),
            "shared-token".into(),
            ["alpha".into()],
        );
        service.recover().await.unwrap();
        let operation = store
            .get_operation("claim-recovery")
            .await
            .unwrap()
            .unwrap()
            .0;
        assert_eq!(operation.phase, ControllerPhase::Failed);
        assert_eq!(
            operation.response_body.unwrap()["error_code"],
            "recovery_conflict"
        );
        assert!(
            store
                .get_placement(&name())
                .await
                .unwrap()
                .unwrap()
                .0
                .op
                .is_none()
        );
    }

    #[tokio::test]
    async fn report_confirmation_persists_the_standard_replay_envelope() {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(ControllerStore::open(&temp.path().join("controller.db")).unwrap());
        store
            .set_placement(&marked_placement(), 0, "prepare")
            .await
            .unwrap();
        store
            .set_operation(&recovery_operation(ControllerPhase::Unknown), 0)
            .await
            .unwrap();
        let service = ControllerService::new(
            store.clone(),
            Arc::new(StaticClient(RemoteOutcome::Ambiguous("unused".into()))),
            "shared-token".into(),
            ["alpha".into()],
        );
        service
            .report(
                "bridge-alpha",
                ExecutorReport {
                    schema: REPORT_SCHEMA.into(),
                    name: name(),
                    node: "alpha".into(),
                    generation: 2,
                    state: "stopped".into(),
                    image_present: true,
                    unit_active: "inactive".into(),
                    executor_request_id: Some("ct-controller-op".into()),
                    executor_op_id: Some("executor-op".into()),
                    executor_operation_state: Some(ExecutorTerminalState::Succeeded),
                    executor_error_code: None,
                    reported_at: "2026-08-09T00:00:02Z".into(),
                    operation_token: "shared-token".into(),
                },
            )
            .await
            .unwrap();
        let (operation, _) = store
            .get_operation("claim-recovery")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(operation.phase, ControllerPhase::Succeeded);
        let body = operation.response_body.unwrap();
        assert_eq!(body["schema"], "cosmix.nspawnd.ct-operation.v1");
        assert_eq!(body["request_id"], "recover-1");
        assert_eq!(body["executor"]["outcome"], "confirmed_by_report");
    }

    #[tokio::test]
    async fn report_folding_obeys_terminal_outcome_rules() {
        for (terminal, error_code, observed, expected_phase, marker_retained) in [
            (
                Some(ExecutorTerminalState::Failed),
                Some("state_conflict"),
                "stopped",
                ControllerPhase::Failed,
                false,
            ),
            (
                Some(ExecutorTerminalState::Failed),
                Some("timeout"),
                "stopped",
                ControllerPhase::Succeeded,
                false,
            ),
            (None, None, "stopped", ControllerPhase::Unknown, true),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let store =
                Arc::new(ControllerStore::open(&temp.path().join("controller.db")).unwrap());
            store
                .set_placement(&marked_placement(), 0, "prepare")
                .await
                .unwrap();
            store
                .set_operation(&recovery_operation(ControllerPhase::Unknown), 0)
                .await
                .unwrap();
            let service = ControllerService::new(
                store.clone(),
                Arc::new(StaticClient(RemoteOutcome::Ambiguous("unused".into()))),
                "shared-token".into(),
                ["alpha".into()],
            );
            service
                .report(
                    "bridge-alpha",
                    executor_report(terminal, error_code, observed),
                )
                .await
                .unwrap();
            assert_eq!(
                store
                    .get_operation("claim-recovery")
                    .await
                    .unwrap()
                    .unwrap()
                    .0
                    .phase,
                expected_phase
            );
            assert_eq!(
                store
                    .get_placement(&name())
                    .await
                    .unwrap()
                    .unwrap()
                    .0
                    .op
                    .is_some(),
                marker_retained
            );
        }
    }

    #[test]
    fn schema_valid_but_misbound_replies_are_untrusted() {
        let operation = recovery_operation(ControllerPhase::Unknown);
        assert_eq!(
            classify_remote(
                &operation,
                RemoteOutcome::Reply {
                    rc: RC_OK,
                    body: json!({
                        "schema":EXECUTOR_OPERATION_SCHEMA,
                        "ok":true,
                        "request_id":"different-request",
                        "name":"demo",
                        "op_id":"executor-op",
                        "outcome":"stopped",
                        "observed":"stopped",
                    }),
                },
            )
            .0,
            ControllerPhase::Unknown
        );
        assert!(!valid_executor_status_reply(
            &json!({
                "schema":EXECUTOR_STATUS_SCHEMA,
                "ok":true,
                "name":"different",
                "managed":true,
                "observed":"stopped",
                "current_operation":null,
                "grant_generation":2,
            }),
            &name(),
        ));
        assert!(!valid_executor_request_status_reply(
            &json!({
                "schema":EXECUTOR_REQUEST_STATUS_SCHEMA,
                "ok":true,
                "request_id":"different-request",
                "name":"demo",
                "found":false,
                "in_flight":false,
                "operation":null,
            }),
            &operation,
        ));
    }

    #[test]
    fn remote_classification_preserves_ambiguity() {
        let operation = recovery_operation(ControllerPhase::Unknown);
        assert_eq!(
            classify_remote(
                &operation,
                RemoteOutcome::RejectedBeforeSend("offline".into())
            )
            .0,
            ControllerPhase::Failed
        );
        assert_eq!(
            classify_remote(&operation, RemoteOutcome::Ambiguous("timeout".into())).0,
            ControllerPhase::Unknown
        );
        assert_eq!(
            classify_remote(
                &operation,
                RemoteOutcome::Reply {
                    rc: 20,
                    body: json!({"ok":false,"message":"Mesh bridge error: timeout"})
                }
            )
            .0,
            ControllerPhase::Unknown
        );
        assert_eq!(
            classify_remote(
                &operation,
                RemoteOutcome::Reply {
                    rc: RC_CALLER,
                    body: json!({
                        "schema":EXECUTOR_ERROR_SCHEMA,
                        "ok":false,
                        "request_id":"ct-controller-op",
                        "name":"demo",
                        "error_code":"state_conflict"
                    })
                }
            )
            .0,
            ControllerPhase::Failed
        );
        assert_eq!(
            classify_remote(
                &operation,
                RemoteOutcome::Reply {
                    rc: RC_OK,
                    body: json!({"ok":true})
                }
            )
            .0,
            ControllerPhase::Unknown
        );
    }
}
