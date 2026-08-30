//! Event-driven desired-state reconciliation.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};

use crate::service::NspawnService;
use crate::systemd::SystemdBackend;

const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
const HEALTHY_SESSION_THRESHOLD: Duration = Duration::from_secs(30);

pub async fn run(service: Arc<NspawnService>, backend: Arc<dyn SystemdBackend>) {
    let mut delay = INITIAL_BACKOFF;
    loop {
        let healthy = run_session(&service, &backend).await;
        if healthy {
            delay = INITIAL_BACKOFF;
        } else {
            delay = (delay * 2).min(MAX_BACKOFF);
        }
        // Reconnection backoff is transport recovery, not state polling. On
        // the next iteration streams become ready before the rescan begins.
        tokio::time::sleep(delay).await;
        match backend.reconnect().await {
            Ok(()) => {
                tracing::info!("reconnected to system D-Bus; reinstalling machined streams");
            }
            Err(error) => {
                tracing::warn!(error = %error, "system D-Bus reconnect failed");
            }
        }
    }
}

async fn run_session(service: &Arc<NspawnService>, backend: &Arc<dyn SystemdBackend>) -> bool {
    let (sender, mut receiver) = mpsc::channel(64);
    let (ready_sender, ready_receiver) = oneshot::channel();
    let monitor_backend = backend.clone();
    let monitor =
        tokio::spawn(async move { monitor_backend.monitor_events(sender, ready_sender).await });
    let session_started = Instant::now();
    let monitor_ready = ready_receiver.await.is_ok();
    if monitor_ready {
        match service.reconcile_all("daemon:reconnect").await {
            Ok(()) => {}
            Err(error) => {
                tracing::error!(error = %error.message, code = error.code, "startup/reconnect rescan failed");
            }
        }
        while let Some(event) = receiver.recv().await {
            if let Err(error) = service.handle_machine_event("daemon:event", event).await {
                tracing::error!(error = %error.message, code = error.code, "machine event enforcement failed");
            }
        }
    } else {
        tracing::warn!("machined event monitor ended before signalling readiness");
    }
    match monitor.await {
        Ok(Ok(())) => tracing::warn!("machined event monitor ended"),
        Ok(Err(error)) => tracing::warn!(error = %error, "machined event monitor failed"),
        Err(error) => tracing::warn!(error = %error, "machined event monitor task failed"),
    }
    session_started.elapsed() >= HEALTHY_SESSION_THRESHOLD
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use cosmix_nspawnd::core::{
        DesiredState, INSTANCE_SCHEMA, InstanceName, InstanceRecord, ObservedInstance,
    };

    use super::*;
    use crate::lock::LockManager;
    use crate::store::StateStore;
    use crate::systemd::{BackendError, MachineEvent, Transition};

    struct GapBackend {
        running: Mutex<bool>,
        transitions: Mutex<Vec<Transition>>,
    }

    impl GapBackend {
        fn observed(&self, name: &InstanceName) -> ObservedInstance {
            let running = *self.running.lock().unwrap();
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
    }

    #[async_trait]
    impl SystemdBackend for GapBackend {
        async fn list(&self) -> Result<Vec<ObservedInstance>, BackendError> {
            Ok(Vec::new())
        }

        async fn observe(&self, name: &InstanceName) -> Result<ObservedInstance, BackendError> {
            Ok(self.observed(name))
        }

        async fn transition(
            &self,
            name: &InstanceName,
            transition: Transition,
        ) -> Result<ObservedInstance, BackendError> {
            self.transitions.lock().unwrap().push(transition);
            *self.running.lock().unwrap() = transition == Transition::Start;
            Ok(self.observed(name))
        }

        async fn monitor_events(
            &self,
            sender: mpsc::Sender<MachineEvent>,
            ready: oneshot::Sender<()>,
        ) -> Result<(), BackendError> {
            // The machine appears after both matches are installed but before
            // the rescan begins. With no polling fallback, the ready-before-
            // rescan ordering must catch it here or in the queued event.
            *self.running.lock().unwrap() = true;
            let _ = ready.send(());
            sender
                .send(MachineEvent::New(InstanceName::parse("labspoke").unwrap()))
                .await
                .map_err(|_| BackendError::EventStreamEnded)
        }
    }

    #[tokio::test]
    async fn signal_stream_is_ready_before_rescan_closes_startup_gap() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(temp.path().join("state"), temp.path().join("legacy"));
        store.ensure_layout().unwrap();
        let name = InstanceName::parse("labspoke").unwrap();
        store
            .save_instance(&InstanceRecord {
                schema: INSTANCE_SCHEMA.into(),
                name: name.clone(),
                desired: DesiredState::Stopped,
                updated_at: "now".into(),
                last_operation: None,
            })
            .unwrap();
        let backend = Arc::new(GapBackend {
            running: Mutex::new(false),
            transitions: Mutex::new(Vec::new()),
        });
        let backend_trait: Arc<dyn SystemdBackend> = backend.clone();
        let service = Arc::new(NspawnService::new(
            "alpha".into(),
            store,
            LockManager::new(temp.path().join("locks")),
            backend_trait.clone(),
        ));

        assert!(!run_session(&service, &backend_trait).await);
        assert!(!backend.observed(&name).running);
        assert_eq!(
            backend.transitions.lock().unwrap().as_slice(),
            [Transition::Stop]
        );
    }
}
