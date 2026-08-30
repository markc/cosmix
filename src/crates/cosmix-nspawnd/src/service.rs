//! Host-executor orchestration shared by Bus, one-shot, and reconciliation.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use cosmix_nspawnd::core::{
    CarriedGrant, DesiredState, GenerationRefusal, INSTANCE_SCHEMA, InstanceName, InstanceRecord,
    MutationFingerprint, OPERATION_SCHEMA, ObservedInstance, OperationRecord, OperationState,
    OperationVerb, evaluate_start, request_hash,
};
use serde_json::{Value, json};

use crate::controller::{ExecutorReport, ExecutorTerminalState, REPORT_SCHEMA};
use crate::lock::{LockError, LockHolder, LockManager};
use crate::reporter::ReportTrigger;
use crate::store::{RequestClaim, StateStore, StoreError};
use crate::systemd::{BackendError, MachineEvent, SystemdBackend, Transition};

pub const RC_OK: u8 = 0;
pub const RC_CALLER: u8 = 10;
pub const RC_BACKEND: u8 = 20;
pub const REQUEST_SCHEMA: &str = "cosmix.nspawnd.request.v2";
pub const REQUEST_STATUS_SCHEMA: &str = "cosmix.nspawnd.request-status.v1";

#[derive(Clone, Debug)]
pub struct ApiReply {
    pub rc: u8,
    pub body: Value,
}

impl ApiReply {
    pub fn ok(body: Value) -> Self {
        Self { rc: RC_OK, body }
    }
}

#[derive(Clone, Debug)]
pub struct ApiError {
    pub rc: u8,
    pub code: &'static str,
    pub message: String,
    pub retryable: bool,
    pub op_id: Option<String>,
    pub observed: Option<Box<ObservedInstance>>,
}

impl ApiError {
    pub fn caller(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            rc: RC_CALLER,
            code,
            message: message.into(),
            retryable: false,
            op_id: None,
            observed: None,
        }
    }

    pub fn backend(code: &'static str, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            rc: RC_BACKEND,
            code,
            message: message.into(),
            retryable,
            op_id: None,
            observed: None,
        }
    }

    pub fn body(&self, request_id: Option<&str>) -> Value {
        json!({
            "schema": "cosmix.nspawnd.error.v1",
            "ok": false,
            "error_code": self.code,
            "message": self.message,
            "retryable": self.retryable,
            "request_id": request_id,
            "op_id": self.op_id,
            "observed": self.observed,
        })
    }

    pub fn reply(&self, request_id: Option<&str>) -> ApiReply {
        ApiReply {
            rc: self.rc,
            body: self.body(request_id),
        }
    }

    pub fn body_for(&self, request_id: Option<&str>, name: Option<&InstanceName>) -> Value {
        let mut body = self.body(request_id);
        body["name"] = name.map_or(Value::Null, |name| json!(name));
        body
    }

    pub fn reply_for(&self, request_id: Option<&str>, name: Option<&InstanceName>) -> ApiReply {
        ApiReply {
            rc: self.rc,
            body: self.body_for(request_id, name),
        }
    }
}

impl From<StoreError> for ApiError {
    fn from(error: StoreError) -> Self {
        match &error {
            StoreError::LegacyUnimported(_) => {
                Self::backend("legacy_unimported", error.to_string(), false)
            }
            StoreError::Corrupt { .. } => Self::backend("state_corrupt", error.to_string(), false),
            StoreError::Conflict(_) => Self::caller("state_conflict", error.to_string()),
            StoreError::ReplayEvicted { .. } => Self::caller("replay_evicted", error.to_string()),
            StoreError::Io { .. } => Self::backend("storage_error", error.to_string(), true),
        }
    }
}

impl From<BackendError> for ApiError {
    fn from(error: BackendError) -> Self {
        let (code, retryable) = match &error {
            BackendError::JobFailed { .. } => ("job_failed", false),
            BackendError::Timeout(_) => ("timeout", true),
            BackendError::Postcondition(_) => ("postcondition_failed", false),
            BackendError::EventStreamEnded => ("systemd_unavailable", true),
            BackendError::Dbus(_) => ("systemd_error", true),
            BackendError::OutcomeUnknown(_) => ("outcome_unknown", true),
        };
        Self::backend(code, error.to_string(), retryable)
    }
}

#[derive(Clone, Debug)]
pub struct MutationRequest {
    pub name: InstanceName,
    pub generation: u64,
    pub request_id: String,
    /// Present on authenticated Bus v2 mutations; absent on local
    /// reconciliation, which uses authority already installed on disk.
    pub grant: Option<CarriedGrant>,
}

pub struct NspawnService {
    local_node: String,
    store: StateStore,
    locks: LockManager,
    backend: Arc<dyn SystemdBackend>,
    report_tx: Option<tokio::sync::mpsc::UnboundedSender<ReportTrigger>>,
    in_flight_requests: Arc<Mutex<BTreeMap<(String, String), usize>>>,
}

pub struct InFlightRequestGuard {
    requests: Arc<Mutex<BTreeMap<(String, String), usize>>>,
    key: (String, String),
}

impl Drop for InFlightRequestGuard {
    fn drop(&mut self) {
        let mut requests = self
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(count) = requests.get_mut(&self.key) {
            *count -= 1;
            if *count == 0 {
                requests.remove(&self.key);
            }
        }
    }
}

impl NspawnService {
    pub fn new(
        local_node: String,
        store: StateStore,
        locks: LockManager,
        backend: Arc<dyn SystemdBackend>,
    ) -> Self {
        Self {
            local_node,
            store,
            locks,
            backend,
            report_tx: None,
            in_flight_requests: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub fn with_reporter(
        mut self,
        sender: tokio::sync::mpsc::UnboundedSender<ReportTrigger>,
    ) -> Self {
        self.report_tx = Some(sender);
        self
    }

    pub fn local_node(&self) -> &str {
        &self.local_node
    }

    /// Mark an authenticated Bus mutation in flight before it can wait on the
    /// per-instance lock or backend observation. The guard removes the marker
    /// on every return path, including task cancellation.
    pub fn track_request(&self, actor: &str, request_id: &str) -> InFlightRequestGuard {
        let key = (actor.to_owned(), request_id.to_owned());
        let mut requests = self
            .in_flight_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *requests.entry(key.clone()).or_default() += 1;
        drop(requests);
        InFlightRequestGuard {
            requests: self.in_flight_requests.clone(),
            key,
        }
    }

    pub fn managed_names(&self) -> Result<Vec<InstanceName>, ApiError> {
        self.store.list_managed_names().map_err(Into::into)
    }

    pub async fn report_snapshot(
        &self,
        name: &InstanceName,
        mut request_id: Option<String>,
        mut op_id: Option<String>,
        operation_token: &str,
    ) -> Result<ExecutorReport, ApiError> {
        let correlated = if let Some(candidate_op_id) = op_id.as_deref() {
            self.store
                .load_operation(candidate_op_id)?
                .filter(|operation| {
                    operation.name == *name
                        && operation.actor.starts_with("bridge-")
                        && request_id
                            .as_deref()
                            .is_none_or(|candidate| candidate == operation.request_id)
                })
        } else {
            self.store
                .latest_controller_operation_for(name)?
                .filter(|operation| {
                    request_id
                        .as_deref()
                        .is_none_or(|candidate| candidate == operation.request_id)
                })
        };
        if let Some(operation) = &correlated {
            request_id.get_or_insert_with(|| operation.request_id.clone());
            op_id.get_or_insert_with(|| operation.op_id.clone());
        }
        let (executor_operation_state, executor_error_code) =
            correlated.map_or((None, None), |operation| match operation.state {
                OperationState::Succeeded => (Some(ExecutorTerminalState::Succeeded), None),
                OperationState::Failed => {
                    let error_code = operation
                        .response_body
                        .as_ref()
                        .and_then(|body| body.get("error_code"))
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    if error_code.is_some() {
                        (Some(ExecutorTerminalState::Failed), error_code)
                    } else {
                        (None, None)
                    }
                }
                OperationState::Running => (None, None),
            });
        let observed = self.backend.observe(name).await?;
        let grant = self.store.load_grant(name)?.ok_or_else(|| {
            ApiError::caller(
                "grant_missing",
                "cannot report a managed instance without a grant",
            )
        })?;
        Ok(ExecutorReport {
            schema: REPORT_SCHEMA.into(),
            name: name.clone(),
            node: self.local_node.clone(),
            generation: grant.generation,
            state: observed.state_label().into(),
            image_present: observed.image_present,
            unit_active: observed.unit_active,
            executor_request_id: request_id,
            executor_op_id: op_id,
            executor_operation_state,
            executor_error_code,
            reported_at: now(),
            operation_token: operation_token.into(),
        })
    }

    fn notify_report(&self, name: InstanceName, request_id: Option<String>, op_id: Option<String>) {
        if let Some(sender) = &self.report_tx {
            let _ = sender.send(ReportTrigger::Instance {
                name,
                request_id,
                op_id,
            });
        }
    }

    pub fn notify_all_reports(&self) {
        if let Some(sender) = &self.report_tx {
            let _ = sender.send(ReportTrigger::All);
        }
    }

    pub async fn list(&self) -> Result<Value, ApiError> {
        let observed = self.backend.list().await?;
        let mut by_name = observed
            .into_iter()
            .map(|item| (item.name.clone(), item))
            .collect::<BTreeMap<_, _>>();
        for name in self.store.list_managed_names()? {
            if !by_name.contains_key(&name) {
                by_name.insert(name.clone(), self.backend.observe(&name).await?);
            }
        }
        let mut instances = Vec::new();
        for (name, observed) in by_name {
            instances.push(self.status_value(&name, observed)?);
        }
        Ok(json!({
            "schema": "cosmix.nspawnd.list.v1",
            "ok": true,
            "host": self.local_node,
            "observed_at": now(),
            "instances": instances,
        }))
    }

    pub async fn status(&self, name: &InstanceName) -> Result<Value, ApiError> {
        let observed = self.backend.observe(name).await?;
        self.status_value(name, observed)
    }

    fn status_value(
        &self,
        name: &InstanceName,
        observed: ObservedInstance,
    ) -> Result<Value, ApiError> {
        let grant = self.store.load_grant(name)?;
        let tombstone = self.store.load_tombstone(name)?;
        let instance = self.store.load_instance(name)?;
        let latest = self.store.latest_operation_for(name)?;
        let start = grant.as_ref().map_or_else(
            || Err(GenerationRefusal::GrantMissing),
            |grant| {
                evaluate_start(
                    name,
                    grant.generation,
                    &self.local_node,
                    Some(grant),
                    tombstone.as_ref(),
                )
            },
        );
        let current_operation = latest
            .as_ref()
            .filter(|operation| operation.state == OperationState::Running);
        let last_operation = latest
            .as_ref()
            .filter(|operation| operation.state != OperationState::Running);
        Ok(json!({
            "schema": "cosmix.nspawnd.status.v1",
            "ok": true,
            "host": self.local_node,
            "name": name,
            "managed": instance.is_some(),
            "observed": observed.state_label(),
            "desired": instance.as_ref().map(|value| value.desired.as_str()),
            "image_present": observed.image_present,
            "machine": observed,
            "grant": grant,
            "grant_generation": grant.as_ref().map(|value| value.generation),
            "tombstone": tombstone,
            "minimum_generation": tombstone.as_ref().map_or(1, |value| value.minimum_generation),
            "start_permitted": start.is_ok(),
            "start_refusal": start.err().map(|error| error.to_string()),
            "current_operation": current_operation,
            "last_operation": last_operation,
            "observed_at": now(),
        }))
    }

    pub async fn start(&self, actor: &str, request: MutationRequest) -> Result<ApiReply, ApiError> {
        self.mutate(actor, request, OperationVerb::Start, false)
            .await
    }

    pub async fn stop(&self, actor: &str, request: MutationRequest) -> Result<ApiReply, ApiError> {
        self.mutate(actor, request, OperationVerb::Stop, false)
            .await
    }

    pub fn request_status(
        &self,
        actor: &str,
        request_id: &str,
        name: &InstanceName,
    ) -> Result<Value, ApiError> {
        let operation = self.store.find_request(actor, request_id)?;
        let in_flight = self
            .in_flight_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&(actor.to_owned(), request_id.to_owned()));
        Ok(json!({
            "schema": REQUEST_STATUS_SCHEMA,
            "ok": true,
            "actor": actor,
            "request_id": request_id,
            "name": name,
            "found": operation.is_some(),
            "in_flight": in_flight,
            "operation": operation,
        }))
    }

    async fn mutate(
        &self,
        actor: &str,
        request: MutationRequest,
        verb: OperationVerb,
        reconcile_bypass_stop_grant: bool,
    ) -> Result<ApiReply, ApiError> {
        let hash = request_hash(&MutationFingerprint {
            schema: REQUEST_SCHEMA,
            verb,
            name: &request.name,
            generation: request.generation,
            request_id: &request.request_id,
            grant: request.grant.as_ref(),
        })
        .map_err(|error| ApiError::caller("invalid_request", error.to_string()))?;
        if let Some(reply) = self.replay(actor, &request.request_id, &hash)? {
            return Ok(reply);
        }

        if let Some(carried) = request.grant.clone() {
            carried
                .validate_for(&request.name, &self.local_node, request.generation)
                .map_err(|error| ApiError::caller("invalid_grant", error))?;
            if let Some(tombstone) = self.store.load_tombstone(&request.name)?
                && request.generation < tombstone.minimum_generation
            {
                return Err(ApiError::caller(
                    "tombstone_floor",
                    format!(
                        "generation {} is below tombstone minimum {}",
                        request.generation, tombstone.minimum_generation
                    ),
                ));
            }
        }

        let op_id = ulid::Ulid::new().to_string();
        let holder = LockHolder {
            op_id: op_id.clone(),
            verb: verb.bus_name().into(),
            actor: actor.into(),
            pid: std::process::id(),
            started_at: now(),
        };
        let _lock = self
            .locks
            .acquire(&request.name, holder)
            .map_err(lock_error)?;
        if let Some(reply) = self.replay(actor, &request.request_id, &hash)? {
            return Ok(reply);
        }

        let before = self.backend.observe(&request.name).await?;
        let started_at = now();
        let mut operation = OperationRecord {
            schema: OPERATION_SCHEMA.into(),
            op_id: op_id.clone(),
            actor: actor.into(),
            request_id: request.request_id.clone(),
            request_hash: hash,
            verb,
            name: request.name.clone(),
            generation: request.generation.max(1),
            state: OperationState::Running,
            started_at: started_at.clone(),
            completed_at: None,
            observed_before: before.clone(),
            observed_after: None,
            response_rc: None,
            response_body: None,
        };
        match self.store.claim_operation(&operation)? {
            RequestClaim::Claimed => {}
            RequestClaim::Existing(existing) => {
                return replay_operation(*existing, &operation.request_hash);
            }
        }
        let preparation = (|| -> Result<(), ApiError> {
            if let Some(carried) = request.grant.clone() {
                self.store.merge_grant(carried.into_grant(now()))?;
            }
            match verb {
                OperationVerb::Start | OperationVerb::ReconcileStart => {
                    let grant = self.store.load_grant(&request.name)?;
                    let tombstone = self.store.load_tombstone(&request.name)?;
                    evaluate_start(
                        &request.name,
                        request.generation,
                        &self.local_node,
                        grant.as_ref(),
                        tombstone.as_ref(),
                    )
                    .map_err(generation_error)?;
                }
                OperationVerb::Stop | OperationVerb::ReconcileStop
                    if !reconcile_bypass_stop_grant =>
                {
                    let grant = self.store.load_grant(&request.name)?;
                    let grant = grant.as_ref().ok_or_else(|| {
                        ApiError::caller("grant_missing", "stop requires a local grant")
                    })?;
                    if grant.owner != self.local_node {
                        return Err(ApiError::caller(
                            "owner_mismatch",
                            format!(
                                "grant owner {:?} is not local node {:?}",
                                grant.owner, self.local_node
                            ),
                        ));
                    }
                    if grant.generation != request.generation {
                        return Err(ApiError::caller(
                            "grant_mismatch",
                            format!(
                                "expected generation {} does not match local grant {}",
                                request.generation, grant.generation
                            ),
                        ));
                    }
                }
                OperationVerb::Stop | OperationVerb::ReconcileStop => {}
            }
            Ok(())
        })();
        if let Err(mut error) = preparation {
            error.op_id = Some(op_id.clone());
            let body = error.body_for(Some(&request.request_id), Some(&request.name));
            operation.state = OperationState::Failed;
            operation.completed_at = Some(now());
            operation.observed_after = Some(before);
            operation.response_rc = Some(error.rc);
            operation.response_body = Some(body);
            self.store.save_operation(&operation)?;
            self.notify_report(request.name, Some(request.request_id), Some(op_id));
            return Err(error);
        }
        let transition_target = match verb {
            OperationVerb::Start | OperationVerb::ReconcileStart => DesiredState::Running,
            OperationVerb::Stop | OperationVerb::ReconcileStop => DesiredState::Stopped,
        };
        if matches!(verb, OperationVerb::Start | OperationVerb::Stop) {
            self.store.save_instance(&InstanceRecord {
                schema: INSTANCE_SCHEMA.into(),
                name: request.name.clone(),
                desired: transition_target,
                updated_at: now(),
                last_operation: Some(op_id.clone()),
            })?;
        }
        let durable_desired = match self.store.load_instance(&request.name) {
            Ok(Some(value)) => value.desired,
            Ok(None) => transition_target,
            Err(_) if verb == OperationVerb::ReconcileStop => {
                // Reconciliation must still be able to drive a fail-closed
                // stop when the desired-state record itself is corrupt. It
                // does not rewrite that record.
                transition_target
            }
            Err(error) => return Err(error.into()),
        };

        let already_settled = match transition_target {
            DesiredState::Running => before.running && before.unit_active == "active",
            DesiredState::Stopped => {
                !before.running
                    && matches!(
                        before.unit_active.as_str(),
                        "inactive" | "failed" | "not-found"
                    )
            }
        };
        let transition = match transition_target {
            DesiredState::Running => Transition::Start,
            DesiredState::Stopped => Transition::Stop,
        };
        let outcome = if already_settled {
            Ok(before.clone())
        } else {
            self.backend
                .transition(&request.name, transition)
                .await
                .map_err(ApiError::from)
        };

        match outcome {
            Ok(after) => {
                let label = match (transition_target, already_settled) {
                    (DesiredState::Running, true) => "already_running",
                    (DesiredState::Running, false) => "started",
                    (DesiredState::Stopped, true) => "already_stopped",
                    (DesiredState::Stopped, false) => "stopped",
                };
                let body = json!({
                    "schema": "cosmix.nspawnd.operation.v1",
                    "ok": true,
                    "verb": verb.bus_name(),
                    "host": self.local_node,
                    "name": request.name,
                    "generation": request.generation,
                    "request_id": request.request_id,
                    "op_id": op_id,
                    "outcome": label,
                    "desired": durable_desired.as_str(),
                    "transition_target": transition_target.as_str(),
                    "observed": after.state_label(),
                    "machine": after,
                    "started_at": started_at,
                    "completed_at": now(),
                });
                operation.state = OperationState::Succeeded;
                operation.completed_at = body["completed_at"].as_str().map(str::to_owned);
                operation.observed_after = Some(after);
                operation.response_rc = Some(RC_OK);
                operation.response_body = Some(body.clone());
                self.store.save_operation(&operation)?;
                if let Err(error) = self.store.gc_operations() {
                    tracing::error!(error = %error, "completed-operation GC failed");
                }
                drop(_lock);
                self.notify_report(request.name, Some(request.request_id), Some(op_id));
                Ok(ApiReply::ok(body))
            }
            Err(mut error) => {
                error.op_id = Some(op_id);
                // Diagnostic-only second observation for the error envelope;
                // never policy input. The systemd transition's ONE-read
                // contract applies to its positive success postcondition.
                error.observed = self.backend.observe(&request.name).await.ok().map(Box::new);
                let body = error.body_for(Some(&request.request_id), Some(&request.name));
                operation.state = OperationState::Failed;
                operation.completed_at = Some(now());
                operation.observed_after = error.observed.as_deref().cloned();
                operation.response_rc = Some(error.rc);
                operation.response_body = Some(body);
                self.store.save_operation(&operation)?;
                if let Err(gc_error) = self.store.gc_operations() {
                    tracing::error!(error = %gc_error, "completed-operation GC failed");
                }
                drop(_lock);
                self.notify_report(request.name, Some(request.request_id), error.op_id.clone());
                Err(error)
            }
        }
    }

    fn replay(
        &self,
        actor: &str,
        request_id: &str,
        hash: &str,
    ) -> Result<Option<ApiReply>, ApiError> {
        let Some(operation) = self.store.find_request(actor, request_id)? else {
            return Ok(None);
        };
        replay_operation(operation, hash).map(Some)
    }

    pub async fn reconcile_all(&self, actor: &str) -> Result<(), ApiError> {
        let mut first_error = None;
        for name in self.store.list_managed_names()? {
            if let Err(error) = self.reconcile_one(actor, &name).await {
                tracing::error!(instance = %name, error = %error.message, code = error.code, "instance reconciliation failed closed");
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        self.notify_all_reports();
        first_error.map_or(Ok(()), Err)
    }

    pub async fn handle_machine_event(
        &self,
        actor: &str,
        event: MachineEvent,
    ) -> Result<(), ApiError> {
        match event {
            MachineEvent::New(name) => {
                let result = self.enforce_appearance(actor, &name).await;
                self.notify_report(name, None, None);
                result
            }
            MachineEvent::Removed(name) => {
                tracing::info!(instance = %name, "machined reported instance removed; no blind restart outside startup/reconnect reconciliation");
                self.notify_report(name, None, None);
                Ok(())
            }
        }
    }

    async fn reconcile_one(&self, actor: &str, name: &InstanceName) -> Result<(), ApiError> {
        let observed = self.backend.observe(name).await?;
        let instance = match self.store.load_instance(name) {
            Ok(Some(instance)) => instance,
            Ok(None) => return Ok(()),
            Err(error) => {
                let error = ApiError::from(error);
                if observed.running {
                    self.force_reconcile_stop(actor, name).await?;
                }
                return Err(error);
            }
        };
        match instance.desired {
            DesiredState::Running if observed.running => {
                let refusal = match (self.store.load_grant(name), self.store.load_tombstone(name)) {
                    (Ok(Some(grant)), Ok(tombstone)) => evaluate_start(
                        name,
                        grant.generation,
                        &self.local_node,
                        Some(&grant),
                        tombstone.as_ref(),
                    )
                    .err()
                    .map(|error| error.to_string()),
                    (Ok(None), _) => Some("grant missing".into()),
                    (Err(error), _) | (_, Err(error)) => Some(error.to_string()),
                };
                if let Some(refusal) = refusal {
                    tracing::warn!(instance = %name, refusal = %refusal, "desired-running instance is fenced; forcing stop while preserving desired state");
                    self.force_reconcile_stop(actor, name).await?;
                }
            }
            DesiredState::Running if !observed.running => {
                let grant = self.store.load_grant(name)?;
                let generation = grant.as_ref().map_or(0, |value| value.generation);
                let request = MutationRequest {
                    name: name.clone(),
                    generation,
                    request_id: format!("reconcile-{}", ulid::Ulid::new()),
                    grant: None,
                };
                self.mutate(actor, request, OperationVerb::ReconcileStart, false)
                    .await?;
            }
            DesiredState::Stopped if observed.running => {
                self.force_reconcile_stop(actor, name).await?;
            }
            _ => {}
        }
        Ok(())
    }

    async fn enforce_appearance(&self, actor: &str, name: &InstanceName) -> Result<(), ApiError> {
        let instance = match self.store.load_instance(name) {
            Ok(Some(instance)) => instance,
            Ok(None) => return Ok(()),
            Err(error) => {
                tracing::error!(instance = %name, error = %error, "managed instance record is invalid; forcing stop");
                return self.force_reconcile_stop(actor, name).await.map(|_| ());
            }
        };
        let valid_running = if instance.desired == DesiredState::Running {
            match (self.store.load_grant(name), self.store.load_tombstone(name)) {
                (Ok(Some(grant)), Ok(tombstone)) => evaluate_start(
                    name,
                    grant.generation,
                    &self.local_node,
                    Some(&grant),
                    tombstone.as_ref(),
                )
                .is_ok(),
                _ => false,
            }
        } else {
            false
        };
        if valid_running {
            return Ok(());
        }
        self.force_reconcile_stop(actor, name).await.map(|_| ())
    }

    async fn force_reconcile_stop(
        &self,
        actor: &str,
        name: &InstanceName,
    ) -> Result<ApiReply, ApiError> {
        let generation = self
            .store
            .load_grant(name)
            .ok()
            .flatten()
            .as_ref()
            .map_or(1, |value| value.generation);
        self.mutate(
            actor,
            MutationRequest {
                name: name.clone(),
                generation,
                request_id: format!("enforce-{}", ulid::Ulid::new()),
                grant: None,
            },
            OperationVerb::ReconcileStop,
            true,
        )
        .await
    }
}

pub fn startup_maintenance(
    store: &StateStore,
    locks: &LockManager,
) -> Result<(usize, usize), ApiError> {
    let mut interrupted = 0;
    for snapshot in store.running_operations()? {
        let holder = LockHolder {
            op_id: format!("recover-{}", snapshot.op_id),
            verb: "nspawnd.recover-interrupted".into(),
            actor: "daemon:startup".into(),
            pid: std::process::id(),
            started_at: now(),
        };
        let _lock = match locks.acquire(&snapshot.name, holder) {
            Ok(lock) => lock,
            Err(LockError::Busy { holder }) => {
                tracing::warn!(
                    instance = %snapshot.name,
                    operation = %snapshot.op_id,
                    holder_operation = ?holder.as_ref().map(|value| value.op_id.as_str()),
                    "startup recovery skipped an operation held by a live executor"
                );
                continue;
            }
            Err(error) => return Err(lock_error(error)),
        };
        let Some(mut operation) = store.load_operation(&snapshot.op_id)? else {
            continue;
        };
        if operation.state != OperationState::Running {
            continue;
        }
        let mut error = ApiError::backend(
            "interrupted",
            "operation outcome is unknown because the executor restarted before recording completion; retry with a new request_id",
            true,
        );
        error.op_id = Some(operation.op_id.clone());
        operation.state = OperationState::Failed;
        operation.completed_at = Some(now());
        operation.response_rc = Some(error.rc);
        operation.response_body =
            Some(error.body_for(Some(&operation.request_id), Some(&operation.name)));
        store.save_operation(&operation)?;
        interrupted += 1;
    }
    let removed = store.gc_operations()?;
    Ok((interrupted, removed))
}

fn replay_operation(operation: OperationRecord, hash: &str) -> Result<ApiReply, ApiError> {
    if operation.request_hash != hash {
        return Err(ApiError::caller(
            "request_conflict",
            format!(
                "actor/request_id already maps to op {} with different content",
                operation.op_id
            ),
        ));
    }
    match (operation.response_rc, operation.response_body) {
        (Some(rc), Some(body)) => Ok(ApiReply { rc, body }),
        _ => {
            let mut error = ApiError::caller(
                "operation_in_progress",
                format!("request is already running as op {}", operation.op_id),
            );
            error.retryable = true;
            error.op_id = Some(operation.op_id);
            Err(error)
        }
    }
}

fn generation_error(error: GenerationRefusal) -> ApiError {
    let code = match error {
        GenerationRefusal::GrantMissing => "grant_missing",
        GenerationRefusal::NameMismatch { .. } => "grant_mismatch",
        GenerationRefusal::OwnerMismatch { .. } => "owner_mismatch",
        GenerationRefusal::GrantMismatch { .. } => "grant_mismatch",
        GenerationRefusal::Fenced { .. } => "fenced",
    };
    ApiError::caller(code, error.to_string())
}

fn lock_error(error: LockError) -> ApiError {
    match error {
        LockError::Busy { holder } => {
            let mut error = ApiError::caller("busy", "instance operation lock is held");
            error.retryable = true;
            error.op_id = holder.map(|holder| holder.op_id);
            error
        }
        LockError::Io { .. } | LockError::Serialise(_) => {
            ApiError::backend("lock_error", error.to_string(), true)
        }
    }
}

fn now() -> String {
    cosmix_buildinfo::now_rfc3339()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use cosmix_nspawnd::core::{
        CARRIED_GRANT_SCHEMA, CarriedGrant, GRANT_SCHEMA, Grant, GrantSource, TOMBSTONE_SCHEMA,
        Tombstone, TombstoneSource,
    };
    use tokio::sync::mpsc;

    use super::*;

    #[derive(Default)]
    struct FakeBackend {
        observed: Mutex<BTreeMap<InstanceName, ObservedInstance>>,
        transitions: Mutex<Vec<Transition>>,
    }

    impl FakeBackend {
        fn set(&self, observed: ObservedInstance) {
            self.observed
                .lock()
                .unwrap()
                .insert(observed.name.clone(), observed);
        }
    }

    #[async_trait]
    impl SystemdBackend for FakeBackend {
        async fn list(&self) -> Result<Vec<ObservedInstance>, BackendError> {
            Ok(self.observed.lock().unwrap().values().cloned().collect())
        }

        async fn observe(&self, name: &InstanceName) -> Result<ObservedInstance, BackendError> {
            Ok(self
                .observed
                .lock()
                .unwrap()
                .get(name)
                .cloned()
                .unwrap_or_else(|| observed(name, false)))
        }

        async fn transition(
            &self,
            name: &InstanceName,
            transition: Transition,
        ) -> Result<ObservedInstance, BackendError> {
            self.transitions.lock().unwrap().push(transition);
            let value = observed(name, transition == Transition::Start);
            self.set(value.clone());
            Ok(value)
        }

        async fn monitor_events(
            &self,
            _sender: mpsc::Sender<MachineEvent>,
            ready: tokio::sync::oneshot::Sender<()>,
        ) -> Result<(), BackendError> {
            let _ = ready.send(());
            std::future::pending().await
        }
    }

    fn name() -> InstanceName {
        InstanceName::parse("labspoke").unwrap()
    }

    fn observed(name: &InstanceName, running: bool) -> ObservedInstance {
        ObservedInstance {
            name: name.clone(),
            running,
            image_present: true,
            machine_class: running.then(|| "container".into()),
            machine_service: running.then(|| "systemd-nspawn".into()),
            machine_unit: running.then(|| name.unit_name()),
            unit_load: "loaded".into(),
            unit_active: if running { "active" } else { "inactive" }.into(),
            unit_sub: if running { "running" } else { "dead" }.into(),
            unit_file_state: "disabled".into(),
        }
    }

    fn grant(generation: u64) -> Grant {
        Grant {
            schema: GRANT_SCHEMA.into(),
            name: name(),
            owner: "alpha".into(),
            generation,
            source: GrantSource {
                kind: "test".into(),
                record_version: 7,
                record_state: "complete".into(),
                record_updated: "now".into(),
            },
            installed_at: "now".into(),
        }
    }

    fn carried(generation: u64, owner: &str) -> CarriedGrant {
        CarriedGrant {
            schema: CARRIED_GRANT_SCHEMA.into(),
            name: name(),
            owner: owner.into(),
            generation,
            record_version: 8,
            record_state: "placed".into(),
            record_updated: "2026-08-09T00:00:00Z".into(),
        }
    }

    fn tombstone(floor: u64) -> Tombstone {
        Tombstone {
            schema: TOMBSTONE_SCHEMA.into(),
            name: name(),
            minimum_generation: floor,
            moved_to: "beta".into(),
            op_id: "c0-op".into(),
            recorded_at: "now".into(),
            enforced: true,
            source: TombstoneSource {
                kind: "test".into(),
                advisory_source: false,
            },
        }
    }

    fn instance(desired: DesiredState) -> InstanceRecord {
        InstanceRecord {
            schema: INSTANCE_SCHEMA.into(),
            name: name(),
            desired,
            updated_at: "now".into(),
            last_operation: None,
        }
    }

    fn fixture() -> (tempfile::TempDir, Arc<FakeBackend>, NspawnService) {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(temp.path().join("state"), temp.path().join("legacy"));
        store.ensure_layout().unwrap();
        store.merge_grant(grant(3)).unwrap();
        store.merge_tombstone(tombstone(2)).unwrap();
        store
            .save_instance(&instance(DesiredState::Stopped))
            .unwrap();
        let backend = Arc::new(FakeBackend::default());
        backend.set(observed(&name(), false));
        let backend_trait: Arc<dyn SystemdBackend> = backend.clone();
        let service = NspawnService::new(
            "alpha".into(),
            store,
            LockManager::new(temp.path().join("locks")),
            backend_trait,
        );
        (temp, backend, service)
    }

    #[tokio::test]
    async fn request_replay_returns_same_result_and_conflicting_content_refuses() {
        let (_temp, backend, service) = fixture();
        let request = MutationRequest {
            name: name(),
            generation: 3,
            request_id: "req-1".into(),
            grant: None,
        };
        let first = service.start("operator", request.clone()).await.unwrap();
        let replay = service.start("operator", request).await.unwrap();
        assert_eq!(first.body, replay.body);
        assert_eq!(
            backend.transitions.lock().unwrap().as_slice(),
            [Transition::Start]
        );

        let conflict = service
            .start(
                "operator",
                MutationRequest {
                    name: name(),
                    generation: 4,
                    request_id: "req-1".into(),
                    grant: None,
                },
            )
            .await
            .unwrap_err();
        assert_eq!(conflict.code, "request_conflict");
    }

    #[test]
    fn post_issue_backend_ambiguity_uses_outcome_unknown_code() {
        let error = ApiError::from(BackendError::OutcomeUnknown("event stream ended".into()));
        assert_eq!(error.code, "outcome_unknown");
        assert!(error.retryable);
    }

    #[tokio::test]
    async fn anti_entropy_report_retains_latest_operation_correlation() {
        let (_temp, _backend, service) = fixture();
        let reply = service
            .stop(
                "bridge-controller",
                MutationRequest {
                    name: name(),
                    generation: 3,
                    request_id: "ct-request-1".into(),
                    grant: None,
                },
            )
            .await
            .unwrap();
        let report = service
            .report_snapshot(&name(), None, None, "shared-token")
            .await
            .unwrap();
        assert_eq!(report.executor_request_id.as_deref(), Some("ct-request-1"));
        assert_eq!(
            report.executor_op_id.as_deref(),
            reply.body["op_id"].as_str()
        );
        assert_eq!(
            report.executor_operation_state,
            Some(ExecutorTerminalState::Succeeded)
        );
        assert_eq!(report.executor_error_code, None);
    }

    #[test]
    fn request_status_exposes_reference_counted_in_flight_dispatches() {
        let (_temp, _backend, service) = fixture();
        assert_eq!(
            service
                .request_status("bridge-controller", "not-claimed", &name())
                .unwrap()["in_flight"],
            false
        );
        let first = service.track_request("bridge-controller", "not-claimed");
        let second = service.track_request("bridge-controller", "not-claimed");
        assert_eq!(
            service
                .request_status("bridge-controller", "not-claimed", &name())
                .unwrap()["in_flight"],
            true
        );
        drop(first);
        assert_eq!(
            service
                .request_status("bridge-controller", "not-claimed", &name())
                .unwrap()["in_flight"],
            true
        );
        drop(second);
        assert_eq!(
            service
                .request_status("bridge-controller", "not-claimed", &name())
                .unwrap()["in_flight"],
            false
        );
    }

    #[tokio::test]
    async fn carried_grant_is_installed_only_when_bound_to_local_executor() {
        let (temp, backend, service) = fixture();
        fs::remove_file(temp.path().join("state/grants/labspoke.json")).unwrap();
        let rejected = service
            .start(
                "bridge-controller",
                MutationRequest {
                    name: name(),
                    generation: 3,
                    request_id: "wrong-owner".into(),
                    grant: Some(carried(3, "beta")),
                },
            )
            .await
            .unwrap_err();
        assert_eq!(rejected.code, "invalid_grant");
        assert!(service.store.load_grant(&name()).unwrap().is_none());
        assert!(backend.transitions.lock().unwrap().is_empty());

        let reply = service
            .start(
                "bridge-controller",
                MutationRequest {
                    name: name(),
                    generation: 3,
                    request_id: "valid-grant".into(),
                    grant: Some(carried(3, "alpha")),
                },
            )
            .await
            .unwrap();
        assert_eq!(reply.rc, RC_OK);
        let installed = service.store.load_grant(&name()).unwrap().unwrap();
        assert_eq!(installed.source.kind, "controller-placement");
        assert_eq!(
            backend.transitions.lock().unwrap().as_slice(),
            [Transition::Start]
        );
    }

    #[tokio::test]
    async fn carried_grant_merge_failure_has_a_durable_request_result() {
        let (_temp, backend, service) = fixture();
        let error = service
            .start(
                "bridge-controller",
                MutationRequest {
                    name: name(),
                    generation: 3,
                    request_id: "grant-conflict".into(),
                    grant: Some(carried(3, "alpha")),
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, "state_conflict");
        let status = service
            .request_status("bridge-controller", "grant-conflict", &name())
            .unwrap();
        assert_eq!(status["found"], true);
        assert_eq!(status["name"], "labspoke");
        assert_eq!(status["operation"]["state"], "failed");
        assert_eq!(
            status["operation"]["response_body"]["error_code"],
            "state_conflict"
        );
        assert_eq!(status["operation"]["response_body"]["name"], "labspoke");
        assert_eq!(
            status["operation"]["response_body"]["request_id"],
            "grant-conflict"
        );
        let report = service
            .report_snapshot(
                &name(),
                Some("grant-conflict".into()),
                status["operation"]["op_id"].as_str().map(str::to_owned),
                "shared-token",
            )
            .await
            .unwrap();
        assert_eq!(
            report.executor_operation_state,
            Some(ExecutorTerminalState::Failed)
        );
        assert_eq!(
            report.executor_error_code.as_deref(),
            Some("state_conflict")
        );
        assert!(backend.transitions.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn dangling_replay_refuses_old_request_allows_fresh_id_and_gc_cleans_residue() {
        let (temp, backend, service) = fixture();
        let original = MutationRequest {
            name: name(),
            generation: 3,
            request_id: "crash-residue".into(),
            grant: None,
        };
        let first = service.start("operator", original.clone()).await.unwrap();
        let op_id = first.body["op_id"].as_str().unwrap();
        fs::remove_file(
            temp.path()
                .join("state/operations")
                .join(format!("{op_id}.json")),
        )
        .unwrap();

        let replay_error = service.start("operator", original).await.unwrap_err();
        assert_eq!(replay_error.code, "replay_evicted");
        assert!(!replay_error.retryable);
        assert!(replay_error.message.contains("new request_id"));
        assert_eq!(
            backend.transitions.lock().unwrap().as_slice(),
            [Transition::Start]
        );

        service
            .start(
                "operator",
                MutationRequest {
                    name: name(),
                    generation: 3,
                    request_id: "fresh-after-residue".into(),
                    grant: None,
                },
            )
            .await
            .unwrap();
        service.store.gc_operations().unwrap();
        assert!(
            service
                .store
                .find_request("operator", "crash-residue")
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn startup_reconciliation_applies_desired_running_and_stopped() {
        let (_temp, backend, service) = fixture();
        service
            .store
            .save_instance(&instance(DesiredState::Running))
            .unwrap();
        service.reconcile_all("daemon:test").await.unwrap();
        assert!(backend.observe(&name()).await.unwrap().running);
        assert_eq!(
            service
                .store
                .load_instance(&name())
                .unwrap()
                .unwrap()
                .desired,
            DesiredState::Running
        );

        service
            .store
            .save_instance(&instance(DesiredState::Stopped))
            .unwrap();
        service.reconcile_all("daemon:test").await.unwrap();
        assert!(!backend.observe(&name()).await.unwrap().running);
        assert_eq!(
            service
                .store
                .load_instance(&name())
                .unwrap()
                .unwrap()
                .desired,
            DesiredState::Stopped
        );
        assert_eq!(
            backend.transitions.lock().unwrap().as_slice(),
            [Transition::Start, Transition::Stop]
        );
    }

    #[tokio::test]
    async fn corrupt_instance_record_allows_reconcile_stop_but_never_start() {
        let (temp, backend, service) = fixture();
        fs::write(
            temp.path().join("state/instances/labspoke.json"),
            b"not json",
        )
        .unwrap();

        let start_error = service
            .mutate(
                "daemon:test",
                MutationRequest {
                    name: name(),
                    generation: 3,
                    request_id: "corrupt-start".into(),
                    grant: None,
                },
                OperationVerb::ReconcileStart,
                false,
            )
            .await
            .unwrap_err();
        assert_eq!(start_error.code, "state_corrupt");
        assert!(backend.transitions.lock().unwrap().is_empty());

        backend.set(observed(&name(), true));
        service
            .mutate(
                "daemon:test",
                MutationRequest {
                    name: name(),
                    generation: 3,
                    request_id: "corrupt-stop".into(),
                    grant: None,
                },
                OperationVerb::ReconcileStop,
                true,
            )
            .await
            .unwrap();
        assert_eq!(
            backend.transitions.lock().unwrap().as_slice(),
            [Transition::Stop]
        );
        assert!(service.store.load_instance(&name()).is_err());
    }

    #[tokio::test]
    async fn startup_reconciliation_stops_running_instance_with_invalid_grant() {
        let (_temp, backend, service) = fixture();
        service
            .store
            .save_instance(&instance(DesiredState::Running))
            .unwrap();
        service.store.merge_tombstone(tombstone(4)).unwrap();
        backend.set(observed(&name(), true));

        service.reconcile_all("daemon:test").await.unwrap();

        assert!(!backend.observe(&name()).await.unwrap().running);
        assert_eq!(
            service
                .store
                .load_instance(&name())
                .unwrap()
                .unwrap()
                .desired,
            DesiredState::Running
        );
        assert_eq!(
            backend.transitions.lock().unwrap().as_slice(),
            [Transition::Stop]
        );

        service.store.merge_grant(grant(4)).unwrap();
        service.reconcile_all("daemon:test").await.unwrap();
        assert!(backend.observe(&name()).await.unwrap().running);
        assert_eq!(
            service
                .store
                .load_instance(&name())
                .unwrap()
                .unwrap()
                .desired,
            DesiredState::Running
        );
    }

    #[tokio::test]
    async fn externally_appearing_fenced_instance_is_stopped() {
        let (_temp, backend, service) = fixture();
        service
            .store
            .save_instance(&instance(DesiredState::Running))
            .unwrap();
        service.store.merge_tombstone(tombstone(4)).unwrap();
        backend.set(observed(&name(), true));
        service
            .handle_machine_event("daemon:event", MachineEvent::New(name()))
            .await
            .unwrap();
        assert!(!backend.observe(&name()).await.unwrap().running);
        assert_eq!(
            service
                .store
                .load_instance(&name())
                .unwrap()
                .unwrap()
                .desired,
            DesiredState::Running
        );
        assert_eq!(
            backend.transitions.lock().unwrap().as_slice(),
            [Transition::Stop]
        );
    }

    #[test]
    fn startup_finalizes_interrupted_operation_and_replay_requires_new_request_id() {
        let (_temp, _backend, service) = fixture();
        let request_id = "interrupted-request";
        let hash = request_hash(&MutationFingerprint {
            schema: REQUEST_SCHEMA,
            verb: OperationVerb::Start,
            name: &name(),
            generation: 3,
            request_id,
            grant: None,
        })
        .unwrap();
        let operation = OperationRecord {
            schema: OPERATION_SCHEMA.into(),
            op_id: "01K00000000000000000000000".into(),
            actor: "operator".into(),
            request_id: request_id.into(),
            request_hash: hash.clone(),
            verb: OperationVerb::Start,
            name: name(),
            generation: 3,
            state: OperationState::Running,
            started_at: "before-restart".into(),
            completed_at: None,
            observed_before: observed(&name(), false),
            observed_after: None,
            response_rc: None,
            response_body: None,
        };
        assert!(matches!(
            service.store.claim_operation(&operation).unwrap(),
            RequestClaim::Claimed
        ));

        assert_eq!(
            startup_maintenance(&service.store, &service.locks)
                .unwrap()
                .0,
            1
        );
        let replay = service
            .replay("operator", request_id, &hash)
            .unwrap()
            .unwrap();
        assert_eq!(replay.rc, RC_BACKEND);
        assert_eq!(replay.body["error_code"], "interrupted");
        assert_eq!(replay.body["retryable"], true);
        assert!(
            replay.body["message"]
                .as_str()
                .unwrap()
                .contains("new request_id")
        );
    }

    #[test]
    fn startup_skips_running_operation_held_by_live_executor() {
        let (_temp, _backend, service) = fixture();
        let operation = OperationRecord {
            schema: OPERATION_SCHEMA.into(),
            op_id: "01K00000000000000000000001".into(),
            actor: "operator".into(),
            request_id: "held-request".into(),
            request_hash: "held-hash".into(),
            verb: OperationVerb::Stop,
            name: name(),
            generation: 3,
            state: OperationState::Running,
            started_at: "before-restart".into(),
            completed_at: None,
            observed_before: observed(&name(), true),
            observed_after: None,
            response_rc: None,
            response_body: None,
        };
        assert!(matches!(
            service.store.claim_operation(&operation).unwrap(),
            RequestClaim::Claimed
        ));
        let held = service
            .locks
            .acquire(
                &name(),
                LockHolder {
                    op_id: "live-op".into(),
                    verb: "nspawnd.stop".into(),
                    actor: "operator".into(),
                    pid: std::process::id(),
                    started_at: now(),
                },
            )
            .unwrap();

        assert_eq!(
            startup_maintenance(&service.store, &service.locks).unwrap(),
            (0, 0)
        );
        assert_eq!(
            service
                .store
                .load_operation(&operation.op_id)
                .unwrap()
                .unwrap()
                .state,
            OperationState::Running
        );

        drop(held);
        assert_eq!(
            startup_maintenance(&service.store, &service.locks)
                .unwrap()
                .0,
            1
        );
    }

    #[tokio::test]
    async fn list_and_status_emit_versioned_success_schemas() {
        let (_temp, _backend, service) = fixture();
        let list = service.list().await.unwrap();
        let status = service.status(&name()).await.unwrap();
        assert_eq!(list["schema"], "cosmix.nspawnd.list.v1");
        assert_eq!(status["schema"], "cosmix.nspawnd.status.v1");
        assert_eq!(status["grant_generation"], 3);
        assert_eq!(status["minimum_generation"], 2);
        assert_eq!(status["start_permitted"], true);
    }
}
