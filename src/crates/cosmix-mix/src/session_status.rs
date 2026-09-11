//! Read-state admission on the existing enrolled connection, never legacy Bus.
use crate::session_state::{self, Source, View};
use cosmix_lib_bus::native_session::*;
use cosmix_lib_client::session::Hello;
use cosmix_lib_client::session::boottime_ms;
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
                && Some(caller.incarnation) == target.parent_incarnation
                && caller.capabilities.contains(&Capability::ReadState);
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
    // Fresh correlated checks refresh the delivered caller's dependency. No cached
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
    // lease.check is recipient-only: broker delivery creates a dependency on
    // the CALLER, not on ourselves. Re-read our own attachment by record ID,
    // retaining request-start time so latency cannot extend its reported lease.
    let Ok(started) = boottime_ms() else {
        return false;
    };
    let Ok(current) = connection.session_self(bound.record_id).await else {
        return false;
    };
    current.record.state == BindingState::Attached
        && Source::from(&current.record) == Source::from(bound)
        && current.record.policy == bound.policy
        && current.record.lease_remaining_ms.is_some_and(|remaining| {
            boottime_ms().is_ok_and(|now| now.saturating_sub(started) < remaining.0)
        })
        && caller_lease.is_none_or(|lease| lease.is_live(hello).unwrap_or(false))
}

/// Runs in the resident's bounded sibling task set, never in its receive arm.
pub(crate) async fn dispatch(
    connection: &VerifiedConnection,
    hello: &Hello,
    bound: &SessionRecord,
    event: &VerifiedCommand,
) {
    let command = event.command();
    if command.id.is_none() {
        return;
    }
    let Some(principal) = event.trusted_context() else {
        refuse(connection, event).await;
        return;
    };
    if !tokio::time::timeout(ADMISSION, admitted(connection, hello, principal, bound))
        .await
        .unwrap_or(false)
    {
        refuse(connection, event).await;
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
                let Some(status) = session_state::view() else {
                    refuse(connection, event).await;
                    return;
                };
                if !connection.client().is_connected() {
                    return;
                }
                // Source changes atomically with the reducer's sequence. Never
                // relabel an old-generation snapshot with a new attachment.
                if status.snapshot.source.as_ref() != Some(&request.target) {
                    refuse(connection, event).await;
                    return;
                }
                let reply = Reply {
                    version: 1,
                    status,
                    capabilities: Capabilities::default(),
                    freshness: "last-observed; CLOCK_BOOTTIME milliseconds since shell state startup (includes suspend); a snapshot is information, never an execution permit",
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

pub(crate) async fn refuse(connection: &VerifiedConnection, event: &VerifiedCommand) {
    if event.command().id.is_some() {
        let _ = tokio::time::timeout(
            ADMISSION,
            connection
                .client()
                .respond(event.command(), 10, r#"{"error_code":"REFUSED"}"#),
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn target() -> SessionRecord {
        SessionRecord {
            name: "test-pane".into(),
            record_assurance: RecordAssurance::SessionBound,
            owner_node: "alpha".into(),
            owner_uid: 1000,
            broker_epoch: HexBytes([1; 16]),
            record_id: HexBytes([2; 16]),
            instance_id: HexBytes([3; 16]),
            incarnation: HexBytes([4; 16]),
            role: Role::PaneShell,
            parent_instance: Some(HexBytes([5; 16])),
            parent_incarnation: Some(HexBytes([6; 16])),
            pane_id: Some(DecimalU64(1)),
            pane_generation: Some(DecimalU64(2)),
            binding_generation: DecimalU64(3),
            state: BindingState::Attached,
            capabilities: vec![Capability::ReadState],
            policy: Policy::Restricted,
            lease_remaining_ms: Some(DecimalU64(1000)),
        }
    }
    fn ambient() -> BrokerPrincipal {
        BrokerPrincipal {
            version: PrincipalVersion::V1,
            assurance: Assurance::LocalUnix,
            owner_node: "alpha".into(),
            unix_uid: 1000,
            unix_gid: 1000,
            peer_pid: 1,
            broker_epoch: HexBytes([1; 16]),
            connection_id: HexBytes([7; 16]),
            session: None,
        }
    }
    #[test]
    fn policy_cross_uid_epoch_and_bound_scope_never_fall_back() {
        let mut target = target();
        let mut caller = ambient();
        assert!(!permitted(&caller, &target));
        target.policy = Policy::DefaultOpen;
        assert!(permitted(&caller, &target));
        caller.unix_uid += 1;
        assert!(!permitted(&caller, &target));
        caller.unix_uid -= 1;
        caller.broker_epoch = HexBytes([9; 16]);
        assert!(!permitted(&caller, &target));
        caller.broker_epoch = target.broker_epoch;
        caller.assurance = Assurance::SessionBound;
        caller.session = Some(SessionIdentity {
            record_id: target.record_id,
            instance_id: target.instance_id,
            incarnation: target.incarnation,
            role: Role::PaneShell,
            parent_instance: target.parent_instance,
            parent_incarnation: target.parent_incarnation,
            pane_id: target.pane_id,
            pane_generation: target.pane_generation,
            binding_generation: target.binding_generation,
            capabilities: vec![Capability::ReadState],
            lease_remaining_ms: DecimalU64(1000),
        });
        assert!(permitted(&caller, &target));
        target.policy = Policy::Restricted;
        assert!(permitted(&caller, &target));
        caller.session.as_mut().unwrap().capabilities.clear();
        assert!(!permitted(&caller, &target));
        target.policy = Policy::DefaultOpen;
        assert!(!permitted(&caller, &target));
        caller
            .session
            .as_mut()
            .unwrap()
            .capabilities
            .push(Capability::ReadState);
        caller.session.as_mut().unwrap().pane_id = Some(DecimalU64(99));
        assert!(!permitted(&caller, &target));
        caller.session.as_mut().unwrap().pane_id = target.pane_id;
        caller.session.as_mut().unwrap().binding_generation = DecimalU64(1);
        assert!(!permitted(&caller, &target));
    }

    #[test]
    fn bounded_typed_request_rejects_unknown_fields_and_bad_counters() {
        let target = Source::from(&target());
        let mut value = serde_json::json!({"version":1,"target":target});
        assert!(serde_json::from_value::<Request>(value.clone()).is_ok());
        value["after_sequence"] = serde_json::json!(12);
        assert!(serde_json::from_value::<Request>(value.clone()).is_err());
        value.as_object_mut().unwrap().remove("after_sequence");
        value["command"] = serde_json::json!("execute");
        assert!(serde_json::from_value::<Request>(value).is_err());
    }
}
