use super::*;

/// Sentinel error: the governor preflight (see `land_one`) found no headroom
/// for the merge-review reservation this landing would need. Distinguished
/// via downcast in [`refine`] from a genuine infrastructure failure, so the
/// caller restores the task to 'done' and moves on to the next task instead
/// of bouncing it (it did nothing wrong) or stopping the whole queue (other
/// tasks may not need review, or need less of it).
#[derive(Debug)]
pub(super) struct GovernorNoHeadroom;

impl std::fmt::Display for GovernorNoHeadroom {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "governor preflight: review reservation cannot fit under daily ceiling \
             — skipping landing this run"
        )
    }
}

impl std::error::Error for GovernorNoHeadroom {}

/// Agent-controlled landing state failed a refinery policy check. This is a
/// task bounce with a durable finding, not an infrastructure error capable of
/// stopping every later task in the queue.
#[derive(Debug)]
pub(super) struct LandingTaskFault {
    pub(super) reason: FindingReason,
    pub(super) detail: String,
}

impl std::fmt::Display for LandingTaskFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for LandingTaskFault {}

#[derive(Debug)]
pub(super) struct LandingInfrastructure(pub(super) String);

impl std::fmt::Display for LandingInfrastructure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for LandingInfrastructure {}

pub(super) fn infrastructure(error: anyhow::Error) -> anyhow::Error {
    if error.downcast_ref::<LandingInfrastructure>().is_some() {
        error
    } else {
        LandingInfrastructure(format!("{error:#}")).into()
    }
}

pub(super) fn infrastructure_message(detail: impl Into<String>) -> anyhow::Error {
    LandingInfrastructure(detail.into()).into()
}

pub(super) fn task_fault(error: anyhow::Error) -> anyhow::Error {
    landing_task_fault(FindingReason::BranchContract, error)
}

pub(super) fn landing_task_fault(reason: FindingReason, error: anyhow::Error) -> anyhow::Error {
    // Classification wrappers are allowed anywhere in the landing path. They
    // must therefore be monotone: once a source has identified host
    // infrastructure, a broader task-content context cannot erase it.
    if error.downcast_ref::<LandingInfrastructure>().is_some() {
        return error;
    }
    LandingTaskFault {
        reason,
        detail: format!("landing policy refused the task branch: {error:#}"),
    }
    .into()
}

pub(super) fn policy_denied(error: anyhow::Error) -> anyhow::Error {
    landing_task_fault(FindingReason::PolicyDenied, error)
}

pub(super) fn bounced_report(task: &Task, detail: String, reason: FindingReason) -> LandingReport {
    LandingReport {
        task_id: task.id,
        branch: task.branch.clone().unwrap_or_else(|| "<missing>".into()),
        profile: task.verifier_profile.clone(),
        landed: false,
        task_status: "bounced",
        detail,
        reason,
        finding_recorded: false,
        ladder_charged: false,
    }
}

pub(super) fn landing_error_report(task: &Task, error: &anyhow::Error) -> LandingReport {
    if let Some(fault) = error.downcast_ref::<LandingTaskFault>() {
        return bounced_report(task, fault.detail.clone(), fault.reason);
    }
    if error.downcast_ref::<LandingInfrastructure>().is_some() {
        return bounced_report(
            task,
            format!("landing infrastructure refused the task: {error:#}"),
            FindingReason::InfraRefusal,
        );
    }
    bounced_report(
        task,
        format!("landing policy refused the task branch: {error:#}"),
        FindingReason::BranchContract,
    )
}

#[cfg(test)]
thread_local! {
    pub(super) static FAIL_NEXT_LANDING_UNANNOTATED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    pub(super) static FAIL_NEXT_WORKSPACE_WRITE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    pub(super) static FAIL_NEXT_LOCKFILE_READ: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
