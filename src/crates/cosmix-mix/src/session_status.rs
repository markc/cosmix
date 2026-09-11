//! Read-state admission on the existing enrolled connection, never legacy Bus.
use crate::session_state::{self, Source, View};
use cosmix_lib_bus::native_session::*;
use cosmix_lib_client::session::Hello;
use cosmix_lib_client::{VerifiedCommand, VerifiedConnection};
use serde::{Deserialize, Serialize};
use std::time::Duration;

const MAX_REQUEST: usize = 2048;
const ADMISSION: Duration = Duration::from_secs(2);
pub(crate) const VERB: &str = "shell.status";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u8,
    target: Source,
    #[serde(default)]
    after_sequence: Option<DecimalU64>,
}

#[derive(Serialize)]
struct Capabilities {
    shell_phase: &'static str,
    cwd: &'static str,
    prompt_generation: &'static str,
    jobs: &'static str,
    job_signal: &'static str,
    foreground: &'static str,
    evaluation_submit: &'static str,
    evaluation_inspect: &'static str,
    input: &'static str,
    isolated_task: &'static str,
    events: &'static str,
}
impl Default for Capabilities {
    fn default() -> Self {
        Self {
            shell_phase: "snapshot",
            cwd: "last-observed-snapshot",
            prompt_generation: "snapshot",
            jobs: "UNSUPPORTED",
            job_signal: "UNSUPPORTED",
            foreground: "UNSUPPORTED",
            evaluation_submit: "UNSUPPORTED",
            evaluation_inspect: "UNSUPPORTED",
            input: "UNSUPPORTED",
            isolated_task: "UNSUPPORTED",
            events: "UNSUPPORTED",
        }
    }
}
#[derive(Serialize)]
struct Reply {
    version: u8,
    status: View,
    capabilities: Capabilities,
    freshness: &'static str,
}

/// Pure policy step, called only with a broker-authenticated stamp. Bound
/// principals never fall back to ambient owner authority on the same connection.
fn permitted(principal: &BrokerPrincipal, target: &SessionRecord) -> bool {
    if principal.broker_epoch != target.broker_epoch
        || principal.unix_uid != target.owner_uid
        || principal.owner_node != target.owner_node
    {
        return false;
    }
    match (principal.assurance, &principal.session) {
        (Assurance::LocalUnix, None) => target.policy == Policy::DefaultOpen,
        (Assurance::SessionBound, Some(caller)) => {
            let parent = caller.role == Role::Term
                && Some(caller.instance_id) == target.parent_instance
                && Some(caller.incarnation) == target.parent_incarnation;
            let own_pane = caller.role == Role::PaneShell
                && caller.record_id == target.record_id
                && caller.instance_id == target.instance_id
                && caller.incarnation == target.incarnation
                && caller.binding_generation == target.binding_generation
                && caller.pane_id == target.pane_id
                && caller.pane_generation == target.pane_generation
                && caller.capabilities.contains(&Capability::ReadState);
            parent || own_pane
        }
        _ => false,
    }
}

async fn admitted(
    connection: &VerifiedConnection,
    hello: &Hello,
    principal: &BrokerPrincipal,
    bound: &SessionRecord,
) -> bool {
    if !permitted(principal, bound) {
        return false;
    }
    // Fresh correlated checks also register lifecycle dependencies. No cached
    // discovery snapshot or lease_remaining_ms from the request is authority.
    let caller_lease = if let Some(caller) = &principal.session {
        match connection
            .session_lease_check(RecordRef {
                record_id: caller.record_id,
                incarnation: caller.incarnation,
                binding_generation: caller.binding_generation,
            })
            .await
        {
            Ok(deadline) => Some(deadline),
            Err(_) => return false,
        }
    } else {
        None
    };
    let Ok(target_lease) = connection.session_lease_check(bound.reference()).await else {
        return false;
    };
    target_lease.is_live(hello).unwrap_or(false)
        && caller_lease.is_none_or(|lease| lease.is_live(hello).unwrap_or(false))
}

/// No worker spawn and no evaluator queue. One bounded request is admitted at
/// a time, with an outer timeout so renewals and recovery cannot be starved.
pub(crate) async fn dispatch(
    connection: &VerifiedConnection,
    hello: &Hello,
    bound: &SessionRecord,
    event: &VerifiedCommand,
) {
    let command = event.command();
    let Some(principal) = event.trusted_context() else {
        return;
    };
    if !tokio::time::timeout(ADMISSION, admitted(connection, hello, principal, bound))
        .await
        .unwrap_or(false)
    {
        return;
    }
    let response = if command.command != VERB {
        (10, "{\"error_code\":\"UNSUPPORTED\"}".to_owned())
    } else {
        let request = (command.body.len() <= MAX_REQUEST)
            .then(|| serde_json::from_str::<Request>(&command.body).ok())
            .flatten();
        match request {
            Some(request) if request.version == 1 && request.target == Source::from(bound) => {
                let Some(status) = session_state::view(request.after_sequence) else {
                    return;
                };
                // Source changes atomically with the reducer's sequence. Never
                // relabel an old-generation snapshot with a new attachment.
                if status.snapshot.source.as_ref() != Some(&request.target) {
                    return;
                }
                let reply = Reply {
                    version: 1,
                    status,
                    capabilities: Capabilities::default(),
                    freshness: "last-observed; monotonic milliseconds since shell state startup; a snapshot is information, never an execution permit",
                };
                (
                    0,
                    serde_json::to_string(&reply).expect("bounded snapshot serialises"),
                )
            }
            Some(request) if request.version == 1 => {
                (10, "{\"error_code\":\"STALE_GENERATION\"}".to_owned())
            }
            _ => (10, "{\"error_code\":\"INVALID_REQUEST\"}".to_owned()),
        }
    };
    let _ = tokio::time::timeout(
        ADMISSION,
        connection
            .client()
            .respond(command, response.0, &response.1),
    )
    .await;
}
