---
title: Daemon Lifecycle and Agent Operation
chapter: 10
version: 0.1.0
status: draft
date: 2026-09-05
---

# Daemon lifecycle and agent operation

This chapter separates shipped service infrastructure from the proposed automatic repair and improvement loops. Baseline: public source revision `96d12fdf`; source inspection is not deployment attestation.

## Identity and installation

**DAEMON-001 — Separate identities.** A service's Bus name, operating-system account and process instance are distinct identifiers. Registration does not prove a POSIX identity; a reserved account does not prove that a service is running. Use build provenance and instance information when attributing observations.

**DAEMON-002 — Installation profiles.** System daemons and desktop-session services require different unit profiles. A system unit may use a dedicated service user; a session service inherits the logged-in user's display and audio environment. Do not prescribe `User=` or cross-manager `Requires=` relationships to a user unit. The `interactd` desktop sink is an example of this distinction.

For a managed fixed-identity deployment, retain the existing allocation policy: daemon users and shared credential groups occupy the preferred 500–599 band, citizens the separate 600–699 band. These are deployment preferences, not globally reserved Linux IDs. A collision must fail preflight before identity or ownership changes. Deployed daemon IDs are not recycled; citizen reuse requires explicit retirement, purge evidence and the existing 30-day quarantine. A never-deployed reservation is a separate reclamation decision, not permission to reuse an installed identity.

The concrete account registry, complete hardening matrix and verification rules are retained in the [managed identity profile](10a-daemon-identity-profile.md). Host ownership evidence remains deployment-specific. This chapter does not allocate new IDs or certify an installation from a documented table.

**DAEMON-003 — Ownership.** Configuration and credentials remain operator-controlled; writable service state is distinct from configuration. Shared secret access uses explicitly scoped group membership rather than broad access to another daemon's state directory. Backups and image transfers must verify the destination identity mapping before restoring numeric ownership. Usernames alone do not establish equivalent UID/GID mappings.

Production binaries install under `/opt/cosmix/bin`. Checkout and runtime paths are resolved by the shared [path implementation](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/crates/cosmix-lib-config/src/paths.rs); deployments must account for its overrides and system/user profiles. Historical package examples containing `/usr/local/bin` are not current install instructions.

## Lifecycle and observation

**DAEMON-004 — Bounded lifecycle.** A service must define readiness, degraded operation, drain, shutdown and restart behaviour. Readiness means its advertised function is available, not merely that a process exists. Stop accepting new work before bounded drain; report interrupted work honestly. The shared [shutdown helper](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/crates/cosmix-lib-daemon/src/lib.rs) waits for Ctrl+C or SIGTERM on Unix, but does not itself implement draining, watchdogs or persistence.

**DAEMON-005 — Supervision.** The selected service manager owns process restart. Each shipped unit must specify restart limits, stop deadlines and applicable hardening. A watchdog setting requires a working notification path; a heartbeat is not proof of progress if emitted independently of the operation being monitored. Unit conformance requires inspection of the effective unit and host capabilities, not just template text.

**DAEMON-006 — Evidence.** Expose failures and state transitions through the shared property/activity surfaces. Record actor, operation, causation, outcome and timing without leaking secrets. Preserve an original error or correlation reference when retrying or escalating; changing a health label does not resolve its cause. A lost observer connection is distinguishable from a confirmed service failure.

## Deterministic repair

The [retained repair and improvement profile](10b-repair-improvement-profile.md) preserves the exact action ladder, thresholds, event catalogue, proposal gradient and conformance requirements. Its implementation limitations and conflicts remain explicit; this summary does not waive its detailed obligations.

The retained design is a finite policy-selected action space: restart, reset to known state, fail over, escalate, halt. The general cross-service engine, universal dead-letter queue and timer-wheel contracts from the old repair proposal are **not established as implemented by this audit**. The [index daemon](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/crates/cosmix-indexd/src/main.rs) contains local circuit breakers; that is evidence for one mechanism, not full repair-layer conformance.

**DAEMON-007 — Repair authority.** Automatic repair must use explicit deterministic policy over observed signals. An LLM recommendation does not independently authorise a repair action. Policy declares prerequisites, attempts, backoff, terminal state and reset conditions. Retries of a side-effecting operation require idempotency or an outcome-reconciliation mechanism; a timeout alone does not prove that the operation failed.

**DAEMON-008 — Recovery honesty.** Reset requires an identified recoverable state; failover requires a usable peer and an ownership/fencing rule. Escalation must stop uncontrolled retry churn. Halting requires an effective supervisor inhibit, not an assumption that a daemon can change `Restart=` by exiting. Preserve failure evidence and distinguish recovery from an underlying defect fix.

The old ladder's precise timeout defaults, every-message disk DLQ mandate, global repair topic catalogue and constant-time timer guarantees remain retained intended requirements with unresolved conformance. They require service-specific capacity, privacy and replay decisions before implementation. This summary does not waive them or establish a replacement numeric policy. Chapter 07 owns persistent recovery and migration contracts.

## Agent change loop

**DAEMON-009 — Proposal lifecycle.** The retained intended loop is observe → propose → triage → apply → learn back. Each transition must identify its input, decision and outcome. Replaying a transition must not duplicate an applied effect. An accepted proposal is not evidence that application completed.

**DAEMON-010 — Authority and scope.** Tool discovery and permission to invoke a tool are separate. An invocation must satisfy both the runtime's authority policy and the requested resource scope. A grant must not silently expand because an implementation can reach another resource. Autonomous operation should proceed unattended within granted scope, with correctness checks and structured refusal outside it.

The old L0–L4 proposal gradient and L0/L2/L4 tool projection remain intended requirements, not a claim that all runtimes enforce the same ceiling. L0 is read-only learning, L1 proposal creation, L2 first execution with explicit acceptance, L3 repeat execution bound to approved content, and L4 code-modifying/destructive work with explicit approval per occurrence. Proposal class cannot be silently downgraded. Its prescribed runtime defaults and grant representation need comparison with each runtime. Do not infer permission from a tool's name or its catalogue presence.

**DAEMON-011 — Approval identity.** Where re-execution relies on an approved content hash, approval binds the exact content and scope. A changed skill cannot inherit approval solely by retaining its name. Canonicalisation, grant persistence and revocation must be specified before this mechanism is claimed as delivered. The [skills types](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/crates/cosmix-lib-skills/src/types.rs) and [agent crate](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/crates/cosmix-lib-agent/) are evidence entry points, not certification of the complete proposal loop.

**DAEMON-012 — Independent evaluation.** A producer's own success judgement is insufficient evidence for promotion. Evaluate against external outcomes and task-specific gates. Review budgets may be bounded; an unresolved substantive flaw remains unresolved after the budget expires. The old multi-agent quorum requirement remains conditional on a future decision permitting autonomous L4: at least three independent approvals, any dissent defers to an operator, and an immutable review record. It is dormant, and is not a requirement to revive a retired orchestrator or to prescribe particular model vendors.

## Deployment and change recovery

The [retained NS4 package profile](10c-package-install-profile.md) preserves the full trust-envelope, manifest, phase-ordering, abort and rollback requirements. It is an intended profile with known differences from the current installer, not a runnable deployment recipe or an attested release.

**DAEMON-013 — Verify before promotion.** A release workflow must identify source revision, feature set, artifact identity and compatible target environment. Verify the trust anchor and artifact before executing packaged code. Check account collisions and configuration prerequisites before stopping or replacing the live service. Keep a recoverable previous artifact and define what state changes make binary rollback insufficient.

**DAEMON-014 — Distinguish rollback layers.** Reverting source, swapping a binary and restoring persistent data are different operations. A schema upgrade can prevent an old binary from reading current state. Define compatibility and backup requirements before applying it; reject unsupported downgrades instead of silently destroying data.

The historical NS4 signed tarball/12-phase installer is a retained proposal, not the current universal installer. Current bootstrap/build entry points are [bootstrap](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/bootstrap) and [setup.mix](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/setup.mix). NS4 role packs, exit codes, manifests, distribution tables and synthetic installer gates require an explicit revival decision and implementation audit before use. Dated host examples are not release attestations.

## Conformance and outstanding work

| Contract group | Evidence required |
|---|---|
| DAEMON-001–003 | Registry/profile consistency, collision fixtures, configuration and credential ownership, restore mapping checks |
| DAEMON-004–006 | Effective-unit checks; readiness, signal, drain and interrupted-work tests; recorded target environment |
| DAEMON-007–008 | Fault injection proving bounded retry, idempotency, fencing and terminal-state enforcement |
| DAEMON-009–012 | Replay tests, grant scope/content-change refusal and independent outcome evidence per runtime |
| DAEMON-013–014 | Failed staging, failed readiness, rollback and incompatible-schema fixtures for the actual release workflow |

No tests or host actions were executed for this chapter. Priority work is to identify which contracts each service already satisfies, resolve the user/system-unit profile split, and select a small real repair path before implementing the full historical mechanism.
