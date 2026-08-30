//! Event-driven executor observations with a five-minute anti-entropy backstop.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use cosmix_client::SupervisedClient;
use cosmix_nspawnd::core::InstanceName;
use tokio::sync::mpsc;

use crate::controller::ExecutorReport;
use crate::service::NspawnService;

#[derive(Clone, Debug)]
pub enum ReportTrigger {
    Instance {
        name: InstanceName,
        request_id: Option<String>,
        op_id: Option<String>,
    },
    All,
}

#[derive(Default)]
struct ReportRound {
    all: bool,
    instances: BTreeMap<InstanceName, (Option<String>, Option<String>)>,
}

impl ReportRound {
    fn add(&mut self, trigger: ReportTrigger) {
        match trigger {
            ReportTrigger::All => {
                self.all = true;
                self.instances.clear();
            }
            ReportTrigger::Instance {
                name,
                request_id,
                op_id,
            } if !self.all => {
                self.instances.insert(name, (request_id, op_id));
            }
            ReportTrigger::Instance { .. } => {}
        }
    }
}

pub fn channel() -> (
    mpsc::UnboundedSender<ReportTrigger>,
    mpsc::UnboundedReceiver<ReportTrigger>,
) {
    mpsc::unbounded_channel()
}

pub async fn run(
    mut receiver: mpsc::UnboundedReceiver<ReportTrigger>,
    service: Arc<NspawnService>,
    client: Arc<SupervisedClient>,
    controller_node: String,
    operation_token: String,
) {
    let stagger = blake3::hash(service.local_node().as_bytes()).as_bytes()[0] as u64 % 60;
    let timer = tokio::time::sleep(Duration::from_secs(300 + stagger));
    tokio::pin!(timer);
    loop {
        let trigger = tokio::select! {
            value = receiver.recv() => match value { Some(value) => value, None => break },
            () = &mut timer => {
                timer.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(300));
                ReportTrigger::All
            }
        };
        let mut round = ReportRound::default();
        round.add(trigger);
        while let Ok(trigger) = receiver.try_recv() {
            round.add(trigger);
        }
        let reports = if round.all {
            match service.managed_names() {
                Ok(names) => names
                    .into_iter()
                    .map(|name| (name, None, None))
                    .collect::<Vec<_>>(),
                Err(error) => {
                    tracing::warn!(error = %error.message, "building report snapshot failed");
                    continue;
                }
            }
        } else {
            round
                .instances
                .into_iter()
                .map(|(name, (request_id, op_id))| (name, request_id, op_id))
                .collect()
        };
        for (name, request_id, op_id) in reports {
            if !publish_one(
                &service,
                &client,
                &controller_node,
                &operation_token,
                &name,
                request_id,
                op_id,
            )
            .await
            {
                // A controller outage makes the remainder of this coalesced
                // round redundant. Anti-entropy retries the fleet in >= 5m.
                break;
            }
        }
    }
}

async fn publish_one(
    service: &NspawnService,
    client: &SupervisedClient,
    controller_node: &str,
    operation_token: &str,
    name: &InstanceName,
    request_id: Option<String>,
    op_id: Option<String>,
) -> bool {
    let report: ExecutorReport = match service
        .report_snapshot(name, request_id, op_id, operation_token)
        .await
    {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(instance = %name, error = %error.message, "building executor report failed");
            return true;
        }
    };
    let target = format!("nspawnd.{controller_node}");
    let body = match serde_json::to_value(report) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(instance = %name, error = %error, "encoding executor report failed");
            return true;
        }
    };
    match client.call_typed(&target, "nspawnd.ct.report", body).await {
        Ok(cosmix_bus::PortReply::Ok { .. }) => true,
        Ok(cosmix_bus::PortReply::AppError { rc, message }) => {
            tracing::warn!(instance = %name, rc, message = %message, "controller rejected executor report");
            definitive_controller_error(&message)
        }
        Err(error) => {
            tracing::warn!(instance = %name, error = %error, "executor report delivery is unresolved");
            false
        }
    }
}

fn definitive_controller_error(message: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(message)
        .ok()
        .and_then(|body| {
            body.get("schema")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .as_deref()
        == Some("cosmix.nspawnd.error.v1")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(value: &str) -> InstanceName {
        InstanceName::parse(value).unwrap()
    }

    #[test]
    fn report_round_deduplicates_instances_and_all_subsumes_them() {
        let mut round = ReportRound::default();
        round.add(ReportTrigger::Instance {
            name: name("demo"),
            request_id: Some("old".into()),
            op_id: Some("old-op".into()),
        });
        round.add(ReportTrigger::Instance {
            name: name("demo"),
            request_id: Some("new".into()),
            op_id: Some("new-op".into()),
        });
        assert_eq!(round.instances.len(), 1);
        assert_eq!(round.instances[&name("demo")].0.as_deref(), Some("new"));

        round.add(ReportTrigger::All);
        round.add(ReportTrigger::Instance {
            name: name("other"),
            request_id: None,
            op_id: None,
        });
        assert!(round.all);
        assert!(round.instances.is_empty());
    }

    #[test]
    fn schema_less_bridge_app_error_stops_the_report_round() {
        assert!(!definitive_controller_error(
            r#"{"ok":false,"message":"Mesh bridge error: timeout"}"#
        ));
        assert!(definitive_controller_error(
            r#"{"schema":"cosmix.nspawnd.error.v1","ok":false,"error_code":"auth_denied"}"#
        ));
    }
}
