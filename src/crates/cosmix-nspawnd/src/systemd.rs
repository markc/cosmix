//! Native system D-Bus observation and lifecycle actuation.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use async_trait::async_trait;
use cosmix_nspawnd::core::{InstanceName, ObservedInstance};
use futures_util::StreamExt;
use tokio::sync::{Mutex, RwLock, mpsc, oneshot};
use zbus::zvariant::OwnedObjectPath;
use zbus::{Connection, Proxy};

const MACHINE_DEST: &str = "org.freedesktop.machine1";
const MACHINE_PATH: &str = "/org/freedesktop/machine1";
const MACHINE_MANAGER: &str = "org.freedesktop.machine1.Manager";
const SYSTEMD_DEST: &str = "org.freedesktop.systemd1";
const SYSTEMD_PATH: &str = "/org/freedesktop/systemd1";
const SYSTEMD_MANAGER: &str = "org.freedesktop.systemd1.Manager";
const JOB_TIMEOUT: Duration = Duration::from_secs(130);

type MachineRow = (String, String, String, OwnedObjectPath);
type ImageRow = (String, String, bool, u64, u64, u64, OwnedObjectPath);
type UnitRow = (
    String,
    String,
    String,
    String,
    String,
    String,
    OwnedObjectPath,
    u32,
    String,
    OwnedObjectPath,
);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Transition {
    Start,
    Stop,
}

impl Transition {
    fn method(self) -> &'static str {
        match self {
            Self::Start => "StartUnit",
            Self::Stop => "StopUnit",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MachineEvent {
    New(InstanceName),
    Removed(InstanceName),
}

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("system D-Bus: {0}")]
    Dbus(#[from] zbus::Error),
    #[error("systemd job {job} completed with result {result:?}")]
    JobFailed { job: String, result: String },
    #[error("timed out waiting for systemd job {0}")]
    Timeout(String),
    #[error("positive postcondition failed: {0}")]
    Postcondition(String),
    #[error("machine event stream ended")]
    EventStreamEnded,
    #[error("systemd job outcome is unknown after acceptance: {0}")]
    OutcomeUnknown(String),
}

#[async_trait]
pub trait SystemdBackend: Send + Sync {
    async fn list(&self) -> Result<Vec<ObservedInstance>, BackendError>;
    async fn observe(&self, name: &InstanceName) -> Result<ObservedInstance, BackendError>;
    async fn transition(
        &self,
        name: &InstanceName,
        transition: Transition,
    ) -> Result<ObservedInstance, BackendError>;
    async fn monitor_events(
        &self,
        sender: mpsc::Sender<MachineEvent>,
        ready: oneshot::Sender<()>,
    ) -> Result<(), BackendError>;
    async fn reconnect(&self) -> Result<(), BackendError> {
        Ok(())
    }
}

#[derive(Clone)]
pub struct SystemdDbus {
    connection: std::sync::Arc<RwLock<Connection>>,
    transition_guard: std::sync::Arc<Mutex<()>>,
}

impl SystemdDbus {
    pub async fn connect() -> Result<Self, BackendError> {
        Ok(Self {
            connection: std::sync::Arc::new(RwLock::new(subscribed_connection().await?)),
            transition_guard: std::sync::Arc::new(Mutex::new(())),
        })
    }

    async fn current_connection(&self) -> Connection {
        self.connection.read().await.clone()
    }

    async fn subscribe_jobs(&self) -> Result<DbusJobEvents, BackendError> {
        let connection = self.current_connection().await;
        let (sender, receiver) = mpsc::channel(32);
        let (ready_sender, ready_receiver) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let proxy =
                match Proxy::new(&connection, SYSTEMD_DEST, SYSTEMD_PATH, SYSTEMD_MANAGER).await {
                    Ok(proxy) => proxy,
                    Err(error) => {
                        let _ = ready_sender.send(Err(error));
                        return;
                    }
                };
            let mut stream = match proxy.receive_signal("JobRemoved").await {
                Ok(stream) => stream,
                Err(error) => {
                    let _ = ready_sender.send(Err(error));
                    return;
                }
            };
            if ready_sender.send(Ok(())).is_err() {
                return;
            }
            while let Some(message) = stream.next().await {
                let body = message
                    .body()
                    .deserialize::<(u32, OwnedObjectPath, String, String)>();
                match body {
                    Ok((_id, path, unit, result)) => {
                        if sender.send(JobEvent { path, unit, result }).await.is_err() {
                            return;
                        }
                    }
                    Err(error) => {
                        tracing::warn!(error = %error, "discarding malformed systemd JobRemoved signal");
                    }
                }
            }
        });
        match ready_receiver.await {
            Ok(Ok(())) => Ok(DbusJobEvents { receiver, task }),
            Ok(Err(error)) => Err(BackendError::Dbus(error)),
            Err(_) => Err(BackendError::EventStreamEnded),
        }
    }

    async fn issue_job(
        &self,
        name: &InstanceName,
        transition: Transition,
    ) -> Result<OwnedObjectPath, BackendError> {
        let connection = self.current_connection().await;
        let proxy = Proxy::new(&connection, SYSTEMD_DEST, SYSTEMD_PATH, SYSTEMD_MANAGER).await?;
        let unit = name.unit_name();
        Ok(proxy
            .call(transition.method(), &(unit.as_str(), "replace"))
            .await?)
    }

    async fn observed_map(&self) -> Result<BTreeMap<InstanceName, ObservedInstance>, BackendError> {
        let connection = self.current_connection().await;
        let machine = Proxy::new(&connection, MACHINE_DEST, MACHINE_PATH, MACHINE_MANAGER).await?;
        let machines: Vec<MachineRow> = machine.call("ListMachines", &()).await?;
        let images: Vec<ImageRow> = machine.call("ListImages", &()).await?;

        let (mut names, machine_by_name) = collect_machine_identities(machines);
        let mut image_names = BTreeSet::new();
        for (raw_name, _kind, _read_only, _created, _modified, _usage, _path) in images {
            let Ok(name) = InstanceName::parse(raw_name) else {
                continue;
            };
            image_names.insert(name.clone());
            names.insert(name);
        }

        let systemd = Proxy::new(&connection, SYSTEMD_DEST, SYSTEMD_PATH, SYSTEMD_MANAGER).await?;
        let unit_names = names
            .iter()
            .map(InstanceName::unit_name)
            .collect::<Vec<_>>();
        let unit_refs = unit_names.iter().map(String::as_str).collect::<Vec<_>>();
        let units: Vec<UnitRow> = if unit_refs.is_empty() {
            Vec::new()
        } else {
            systemd.call("ListUnitsByNames", &(unit_refs,)).await?
        };
        let unit_by_name = units
            .into_iter()
            .map(|row| (row.0.clone(), row))
            .collect::<BTreeMap<_, _>>();

        let mut result = BTreeMap::new();
        for name in names {
            let unit_name = name.unit_name();
            let machine = machine_by_name.get(&name);
            let running = machine.is_some_and(|value| value.is_systemd_nspawn);
            let unit = unit_by_name.get(&unit_name);
            let image_present = image_names.contains(&name);
            let unit_file_state = systemd
                .call::<_, _, String>("GetUnitFileState", &(unit_name.as_str(),))
                .await
                .unwrap_or_else(|_| "unknown".into());
            result.insert(
                name.clone(),
                ObservedInstance {
                    name,
                    running,
                    image_present,
                    machine_class: machine.map(|value| value.class.clone()),
                    machine_service: machine.map(|value| value.service.clone()),
                    machine_unit: running.then(|| unit_name.clone()),
                    unit_load: unit.map_or_else(|| "not-found".into(), |row| row.2.clone()),
                    unit_active: unit.map_or_else(|| "inactive".into(), |row| row.3.clone()),
                    unit_sub: unit.map_or_else(|| "dead".into(), |row| row.4.clone()),
                    unit_file_state,
                },
            );
        }
        Ok(result)
    }
}

#[async_trait]
impl SystemdBackend for SystemdDbus {
    async fn list(&self) -> Result<Vec<ObservedInstance>, BackendError> {
        Ok(self.observed_map().await?.into_values().collect())
    }

    async fn observe(&self, name: &InstanceName) -> Result<ObservedInstance, BackendError> {
        Ok(self
            .observed_map()
            .await?
            .remove(name)
            .unwrap_or_else(|| ObservedInstance {
                name: name.clone(),
                running: false,
                image_present: false,
                machine_class: None,
                machine_service: None,
                machine_unit: None,
                unit_load: "not-found".into(),
                unit_active: "inactive".into(),
                unit_sub: "dead".into(),
                unit_file_state: "disabled".into(),
            }))
    }

    async fn transition(
        &self,
        name: &InstanceName,
        transition: Transition,
    ) -> Result<ObservedInstance, BackendError> {
        // Keep a reconnect from swapping the connection between signal
        // subscription and StartUnit/StopUnit issue.
        let _guard = self.transition_guard.lock().await;
        run_job(self, name, transition).await
    }

    async fn monitor_events(
        &self,
        sender: mpsc::Sender<MachineEvent>,
        ready: oneshot::Sender<()>,
    ) -> Result<(), BackendError> {
        let connection = self.current_connection().await;
        let proxy = Proxy::new(&connection, MACHINE_DEST, MACHINE_PATH, MACHINE_MANAGER).await?;
        let mut new_stream = proxy.receive_signal("MachineNew").await?;
        let mut removed_stream = proxy.receive_signal("MachineRemoved").await?;
        let _ = ready.send(());
        loop {
            let (message, is_new) = tokio::select! {
                message = new_stream.next() => (message, true),
                message = removed_stream.next() => (message, false),
            };
            let Some(message) = message else {
                return Err(BackendError::EventStreamEnded);
            };
            let (raw_name, _path) = message.body().deserialize::<(String, OwnedObjectPath)>()?;
            if raw_name == ".host" {
                continue;
            }
            let Ok(name) = InstanceName::parse(raw_name.clone()) else {
                tracing::warn!(machine = %raw_name, "skipping machined event with unsupported instance name");
                continue;
            };
            let event = if is_new {
                MachineEvent::New(name)
            } else {
                MachineEvent::Removed(name)
            };
            if sender.send(event).await.is_err() {
                return Ok(());
            }
        }
    }

    async fn reconnect(&self) -> Result<(), BackendError> {
        let _guard = self.transition_guard.lock().await;
        let connection = subscribed_connection().await?;
        *self.connection.write().await = connection;
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct MachineIdentity {
    class: String,
    service: String,
    is_systemd_nspawn: bool,
}

impl MachineIdentity {
    fn new(name: String, class: String, service: String) -> Self {
        let is_systemd_nspawn = class == "container" && service == "systemd-nspawn";
        if !is_systemd_nspawn {
            tracing::warn!(machine = %name, machine_class = %class, machine_service = %service, "foreign machined entry collides with a supported nspawn name");
        }
        Self {
            class,
            service,
            is_systemd_nspawn,
        }
    }
}

fn collect_machine_identities(
    machines: Vec<MachineRow>,
) -> (
    BTreeSet<InstanceName>,
    BTreeMap<InstanceName, MachineIdentity>,
) {
    let mut names = BTreeSet::new();
    let mut identities = BTreeMap::new();
    for (raw_name, class, service, _path) in machines {
        if raw_name == ".host" {
            continue;
        }
        let Ok(name) = InstanceName::parse(raw_name.clone()) else {
            tracing::warn!(machine = %raw_name, "skipping machined entry with unsupported instance name");
            continue;
        };
        names.insert(name.clone());
        identities.insert(name, MachineIdentity::new(raw_name, class, service));
    }
    (names, identities)
}

async fn subscribed_connection() -> Result<Connection, BackendError> {
    let connection = Connection::system().await?;
    let manager = Proxy::new(&connection, SYSTEMD_DEST, SYSTEMD_PATH, SYSTEMD_MANAGER).await?;
    let _: () = manager.call("Subscribe", &()).await?;
    drop(manager);
    Ok(connection)
}

#[derive(Clone, Debug)]
struct JobEvent {
    path: OwnedObjectPath,
    #[allow(dead_code)]
    unit: String,
    result: String,
}

#[async_trait]
trait JobEvents: Send {
    async fn next(&mut self) -> Option<JobEvent>;
}

struct DbusJobEvents {
    receiver: mpsc::Receiver<JobEvent>,
    task: tokio::task::JoinHandle<()>,
}

#[async_trait]
impl JobEvents for DbusJobEvents {
    async fn next(&mut self) -> Option<JobEvent> {
        self.receiver.recv().await
    }
}

impl Drop for DbusJobEvents {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[async_trait]
trait JobApi: Send + Sync {
    async fn manager_subscription_ready(&self) -> Result<(), BackendError>;
    async fn subscribe_job_removed(&self) -> Result<Box<dyn JobEvents>, BackendError>;
    async fn issue(
        &self,
        name: &InstanceName,
        transition: Transition,
    ) -> Result<OwnedObjectPath, BackendError>;
    async fn observe_after(&self, name: &InstanceName) -> Result<ObservedInstance, BackendError>;
}

#[async_trait]
impl JobApi for SystemdDbus {
    async fn manager_subscription_ready(&self) -> Result<(), BackendError> {
        // `SystemdDbus` only stores connections created by
        // `subscribed_connection`; this method exposes that invariant to the
        // ordering harness used by `run_job`.
        let _connection = self.current_connection().await;
        Ok(())
    }

    async fn subscribe_job_removed(&self) -> Result<Box<dyn JobEvents>, BackendError> {
        Ok(Box::new(self.subscribe_jobs().await?))
    }

    async fn issue(
        &self,
        name: &InstanceName,
        transition: Transition,
    ) -> Result<OwnedObjectPath, BackendError> {
        self.issue_job(name, transition).await
    }

    async fn observe_after(&self, name: &InstanceName) -> Result<ObservedInstance, BackendError> {
        self.observe(name).await
    }
}

async fn run_job(
    api: &dyn JobApi,
    name: &InstanceName,
    transition: Transition,
) -> Result<ObservedInstance, BackendError> {
    // Manager.Subscribe enables JobRemoved delivery on this connection. The
    // signal match is then installed before StartUnit/StopUnit can emit it.
    api.manager_subscription_ready().await?;
    let mut events = api.subscribe_job_removed().await?;
    let job = api.issue(name, transition).await?;
    let job_string = job.to_string();
    let result = tokio::time::timeout(JOB_TIMEOUT, async {
        while let Some(event) = events.next().await {
            if event.path == job {
                return Some(event.result);
            }
        }
        None
    })
    .await
    .map_err(|_| BackendError::Timeout(job_string.clone()))?
    .ok_or_else(|| {
        BackendError::OutcomeUnknown(format!(
            "JobRemoved stream ended while waiting for accepted job {job_string}"
        ))
    })?;
    if result != "done" {
        return Err(BackendError::JobFailed {
            job: job_string,
            result,
        });
    }
    // This is the single positive postcondition read. Any later observation
    // made while constructing an error envelope is diagnostic-only and is
    // never policy input.
    let observed = api.observe_after(name).await.map_err(|error| {
        BackendError::OutcomeUnknown(format!(
            "final observation failed after accepted job {job_string}: {error}"
        ))
    })?;
    let postcondition = match transition {
        Transition::Start => observed.running && observed.unit_active == "active",
        Transition::Stop => {
            !observed.running && matches!(observed.unit_active.as_str(), "inactive" | "failed")
        }
    };
    if !postcondition {
        return Err(BackendError::Postcondition(format!(
            "{} completed but {} is running={} unit_active={:?}",
            transition.method(),
            name,
            observed.running,
            observed.unit_active
        )));
    }
    Ok(observed)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    struct FakeEvents {
        events: Vec<JobEvent>,
    }

    #[async_trait]
    impl JobEvents for FakeEvents {
        async fn next(&mut self) -> Option<JobEvent> {
            if self.events.is_empty() {
                None
            } else {
                Some(self.events.remove(0))
            }
        }
    }

    struct FakeJobApi {
        log: Arc<Mutex<Vec<&'static str>>>,
        job_result: Option<String>,
        observed: ObservedInstance,
        manager_subscribed: bool,
    }

    #[async_trait]
    impl JobApi for FakeJobApi {
        async fn manager_subscription_ready(&self) -> Result<(), BackendError> {
            self.log.lock().unwrap().push("manager-subscribe");
            if self.manager_subscribed {
                Ok(())
            } else {
                Err(BackendError::Postcondition(
                    "systemd manager connection was not subscribed".into(),
                ))
            }
        }

        async fn subscribe_job_removed(&self) -> Result<Box<dyn JobEvents>, BackendError> {
            self.log.lock().unwrap().push("signal-subscribe");
            Ok(Box::new(FakeEvents {
                events: self
                    .job_result
                    .as_ref()
                    .map(|result| JobEvent {
                        path: OwnedObjectPath::try_from("/org/freedesktop/systemd1/job/7").unwrap(),
                        unit: "systemd-nspawn@labspoke.service".into(),
                        result: result.clone(),
                    })
                    .into_iter()
                    .collect(),
            }))
        }

        async fn issue(
            &self,
            _name: &InstanceName,
            _transition: Transition,
        ) -> Result<OwnedObjectPath, BackendError> {
            self.log.lock().unwrap().push("issue");
            Ok(OwnedObjectPath::try_from("/org/freedesktop/systemd1/job/7").unwrap())
        }

        async fn observe_after(
            &self,
            _name: &InstanceName,
        ) -> Result<ObservedInstance, BackendError> {
            self.log.lock().unwrap().push("observe");
            Ok(self.observed.clone())
        }
    }

    fn observed(running: bool, active: &str) -> ObservedInstance {
        ObservedInstance {
            name: InstanceName::parse("labspoke").unwrap(),
            running,
            image_present: true,
            machine_class: Some("container".into()),
            machine_service: Some("systemd-nspawn".into()),
            machine_unit: Some("systemd-nspawn@labspoke.service".into()),
            unit_load: "loaded".into(),
            unit_active: active.into(),
            unit_sub: "x".into(),
            unit_file_state: "disabled".into(),
        }
    }

    #[test]
    fn foreign_machine_names_are_skipped_and_name_collisions_are_not_running() {
        let path = OwnedObjectPath::try_from("/org/freedesktop/machine1/machine/x").unwrap();
        let (names, identities) = collect_machine_identities(vec![
            (
                "UpperForeign".into(),
                "container".into(),
                "systemd-nspawn".into(),
                path.clone(),
            ),
            ("labspoke".into(), "vm".into(), "qemu".into(), path.clone()),
            (
                "labhub".into(),
                "container".into(),
                "systemd-nspawn".into(),
                path,
            ),
        ]);
        assert_eq!(names.len(), 2);
        assert!(!names.iter().any(|name| name.as_str() == "UpperForeign"));
        assert!(!identities[&InstanceName::parse("labspoke").unwrap()].is_systemd_nspawn);
        assert!(identities[&InstanceName::parse("labhub").unwrap()].is_systemd_nspawn);
    }

    #[tokio::test]
    async fn subscribes_before_issue_and_reads_one_postcondition() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let api = FakeJobApi {
            log: log.clone(),
            job_result: Some("done".into()),
            observed: observed(true, "active"),
            manager_subscribed: true,
        };
        run_job(
            &api,
            &InstanceName::parse("labspoke").unwrap(),
            Transition::Start,
        )
        .await
        .unwrap();
        assert_eq!(
            *log.lock().unwrap(),
            ["manager-subscribe", "signal-subscribe", "issue", "observe"]
        );
    }

    #[tokio::test]
    async fn missing_manager_subscription_fails_before_signal_match_or_issue() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let api = FakeJobApi {
            log: log.clone(),
            job_result: Some("done".into()),
            observed: observed(true, "active"),
            manager_subscribed: false,
        };
        assert!(
            run_job(
                &api,
                &InstanceName::parse("labspoke").unwrap(),
                Transition::Start
            )
            .await
            .is_err()
        );
        assert_eq!(*log.lock().unwrap(), ["manager-subscribe"]);
    }

    #[tokio::test]
    async fn failed_job_is_not_treated_as_success() {
        let api = FakeJobApi {
            log: Arc::new(Mutex::new(Vec::new())),
            job_result: Some("failed".into()),
            observed: observed(false, "inactive"),
            manager_subscribed: true,
        };
        assert!(matches!(
            run_job(
                &api,
                &InstanceName::parse("labspoke").unwrap(),
                Transition::Start
            )
            .await,
            Err(BackendError::JobFailed { .. })
        ));
    }

    #[tokio::test]
    async fn event_stream_loss_after_issue_is_outcome_unknown() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let api = FakeJobApi {
            log: log.clone(),
            job_result: None,
            observed: observed(true, "active"),
            manager_subscribed: true,
        };
        assert!(matches!(
            run_job(
                &api,
                &InstanceName::parse("labspoke").unwrap(),
                Transition::Start
            )
            .await,
            Err(BackendError::OutcomeUnknown(_))
        ));
        assert_eq!(
            *log.lock().unwrap(),
            ["manager-subscribe", "signal-subscribe", "issue"]
        );
    }

    #[tokio::test]
    async fn postcondition_mismatch_is_an_error() {
        let api = FakeJobApi {
            log: Arc::new(Mutex::new(Vec::new())),
            job_result: Some("done".into()),
            observed: observed(false, "inactive"),
            manager_subscribed: true,
        };
        assert!(matches!(
            run_job(
                &api,
                &InstanceName::parse("labspoke").unwrap(),
                Transition::Start
            )
            .await,
            Err(BackendError::Postcondition(_))
        ));
    }
}
