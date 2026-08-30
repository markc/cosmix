//! The governor: fleet-level spend control. Per-task budgets live in
//! [`crate::executor::Budget`]; this layer adds the daily ceiling and the
//! kill switch, checked before any claim. Defaults are ON — an unattended
//! fleet must hit a wall by default, not after someone remembers to set one.
//!
//! Two gates: `reserve()` atomically holds headroom for a supervised run
//! (finished spend + live reservations + the request checked and the hold
//! inserted in one transaction — concurrent claims cannot jointly exceed a
//! ceiling), released when the run's actuals land in `runs`. `admit()` is
//! the lighter reservation-aware check for paths that don't own a run
//! lifecycle (MCP claims). A run spanning UTC midnight attributes to its
//! start day. A crashed reservation carrying an owner pid is released by
//! liveness at the next sweep; [`RESERVATION_TTL`] is the fallback for a hold
//! whose owner cannot be evaluated.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;

use crate::ledger::{Ledger, ReservationRefused};

/// Default daily ceilings. Fleet config and one-shot environment overrides
/// are resolved by [`crate::config::FleetPolicy`] (0 disables a ceiling).
pub const DEFAULT_DAILY_BUDGET_USD: f64 = 50.0;
pub const DEFAULT_DAILY_OUTPUT_TOKENS: u64 = 5_000_000;

/// What a run with no explicit caps reserves — an estimate, refined by the
/// actuals the moment the run finishes. Fleet config and one-shot environment
/// overrides are resolved by [`crate::config::FleetPolicy`].
pub const DEFAULT_RESERVE_USD: f64 = 5.0;
pub const DEFAULT_RESERVE_TOKENS: u64 = 500_000;

/// Select the dollar reservation for one task attempt. A task-authored budget
/// replaces the policy default, but a narrower explicit invocation cap keeps
/// its operator authority. No selected hold can exceed the task remainder.
pub fn task_reservation_usd(
    policy_reserve_usd: f64,
    invocation_cap_usd: Option<f64>,
    task_remaining_usd: Option<f64>,
) -> f64 {
    match task_remaining_usd {
        Some(remaining) => invocation_cap_usd
            .map(|cap| cap.min(remaining))
            .unwrap_or(remaining),
        None => invocation_cap_usd.unwrap_or(policy_reserve_usd),
    }
}

/// Expiry fallback for a reservation whose process liveness cannot be checked.
/// Rows carrying a pid are checked at every sweep regardless of age.
pub const RESERVATION_TTL: chrono::Duration = chrono::Duration::hours(4);

/// Kill-switch file, sibling of the ledger. Presence = stopped. A file so
/// both humans and agents can throw it with nothing but `touch`, and so it
/// survives foreman restarts.
pub const STOP_FILE: &str = "STOP";

#[derive(Debug, Clone)]
pub struct Governor {
    pub daily_budget_usd: f64,
    pub daily_output_tokens: u64,
    pub reserve_usd: f64,
    pub reserve_tokens: u64,
    stop_file: PathBuf,
}

#[derive(Debug, Clone)]
pub struct GovernorStatus {
    pub stopped: bool,
    pub spend_today_usd: f64,
    pub output_tokens_today: u64,
    /// Trustworthiness of both run-derived usage totals above, over their
    /// exact UTC-day contributing window.
    pub delivery_void_fraction: crate::ledger::VoidFraction,
    pub reserved_usd: f64,
    pub reserved_tokens: u64,
    pub daily_budget_usd: f64,
    pub daily_output_tokens: u64,
}

fn utc_midnight() -> String {
    Utc::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("midnight exists")
        .and_utc()
        .to_rfc3339()
}

impl Governor {
    /// `ledger_path` anchors the stop file next to the ledger it governs.
    pub fn new(ledger_path: &Path) -> Result<Self> {
        let policy = crate::config::FleetPolicy::load_for_db(ledger_path)?;
        Ok(Self::from_policy(ledger_path, &policy))
    }

    /// Construct from the invocation's already-resolved policy snapshot.
    pub fn from_policy(ledger_path: &Path, policy: &crate::config::FleetPolicy) -> Self {
        let dir = ledger_path.parent().unwrap_or(Path::new("."));
        Governor {
            daily_budget_usd: policy.daily_budget_usd.value,
            daily_output_tokens: policy.daily_output_tokens.value,
            reserve_usd: policy.reserve_usd.value,
            reserve_tokens: policy.reserve_tokens.value,
            stop_file: dir.join(STOP_FILE),
        }
    }

    /// Explicit ceilings (0 disables one), bypassing the env lookups.
    pub fn with_limits(ledger_path: &Path, usd: f64, tokens: u64) -> Self {
        let dir = ledger_path.parent().unwrap_or(Path::new("."));
        Governor {
            daily_budget_usd: usd,
            daily_output_tokens: tokens,
            reserve_usd: DEFAULT_RESERVE_USD,
            reserve_tokens: DEFAULT_RESERVE_TOKENS,
            stop_file: dir.join(STOP_FILE),
        }
    }

    pub fn status(&self, ledger: &Ledger) -> Result<GovernorStatus> {
        // Read paths sweep too — a crashed hold must not pin the ceiling
        // until someone happens to reserve. A failed sweep fails the status:
        // reporting totals known to include unsweepable garbage is worse.
        ledger.sweep_reservations(&(Utc::now() - RESERVATION_TTL).to_rfc3339())?;
        let since = utc_midnight();
        let (spend, tokens) = ledger.usage_since(&since)?;
        let delivery_void_fraction = ledger.delivery_void_fraction_since(&since)?;
        let (reserved_usd, reserved_tokens) = ledger.reserved_totals()?;
        Ok(GovernorStatus {
            stopped: self.stop_file.exists(),
            spend_today_usd: spend,
            output_tokens_today: tokens,
            delivery_void_fraction,
            reserved_usd,
            reserved_tokens,
            daily_budget_usd: self.daily_budget_usd,
            daily_output_tokens: self.daily_output_tokens,
        })
    }

    /// Atomically hold headroom for a run about to start. The hold is the
    /// run's own caps where set, else the estimate defaults — released via
    /// [`Governor::release`] once the actuals are in `runs`.
    pub fn reserve(
        &self,
        ledger: &Ledger,
        claimant: &str,
        task_id: Option<i64>,
        budget: &crate::executor::Budget,
        kind: crate::executor::AgentKind,
    ) -> Result<i64> {
        self.check_stop()?;
        // A lane whose cost the runner discards can never spend against the
        // dollar ceiling, so it must not hold dollars either — holding them
        // gates the free lane behind the metered one's ceiling and stops the
        // fleet dead. Token holds still apply to every lane: those are real
        // for all of them.
        //
        // Requesting $0 is NOT sufficient. The ledger's test is
        // `spent + held + requested > ceiling`, so once spend drifts ABOVE
        // the ceiling — a lowered ceiling, a --no-governor run, or simply
        // actuals overshooting their hold (run 81 cost $5.09 against a $5
        // hold) — even a $0 request is refused and the free lane halts
        // again. Disable the dollar dimension outright for these lanes by
        // passing the ceiling that means "no dollar ceiling", so the
        // guarantee holds at any spend level rather than only below the
        // line.
        let metered = kind.meters_dollars();
        let usd = if metered {
            budget
                .max_budget_usd
                .filter(|v| v.is_finite() && *v >= 0.0)
                .unwrap_or(self.reserve_usd)
        } else {
            0.0
        };
        let dollar_ceiling = if metered { self.daily_budget_usd } else { 0.0 };
        let tokens = budget.max_output_tokens.unwrap_or(self.reserve_tokens);
        ledger.sweep_reservations(&(Utc::now() - RESERVATION_TTL).to_rfc3339())?;
        match ledger.reserve(
            claimant,
            task_id,
            usd,
            tokens,
            dollar_ceiling,
            self.daily_output_tokens,
            &utc_midnight(),
        ) {
            Ok(id) => Ok(id),
            Err(error) if error.downcast_ref::<ReservationRefused>().is_some() => {
                Err(error.context(GovernorReservationRefused))
            }
            Err(error) => Err(error.context("governor")),
        }
    }

    pub fn release(&self, ledger: &Ledger, reservation: i64) -> Result<()> {
        ledger.release_reservation(reservation)
    }

    fn check_stop(&self) -> Result<()> {
        if self.stop_file.exists() {
            anyhow::bail!(
                "governor: kill switch is thrown ({}); `foreman governor resume` to clear",
                self.stop_file.display()
            );
        }
        Ok(())
    }

    /// Check whether a reservation request would fit under the daily ceiling
    /// WITHOUT actually reserving. Returns false if the request cannot fit.
    /// Used for preflight checks to avoid wasting work on reservations that
    /// would fail.
    pub fn check_headroom(
        &self,
        ledger: &Ledger,
        request_usd: f64,
        request_tokens: u64,
    ) -> Result<bool> {
        self.check_headroom_dimensions(ledger, true, request_usd, request_tokens)
    }

    /// Aggregate preflight for a known set of lanes. `check_dollars=false`
    /// mirrors [`Governor::reserve`] for Codex/GLM: those lanes cannot report
    /// dollar spend, so even an already-exceeded Claude dollar ceiling must
    /// not refuse their token-governed reservation.
    pub fn check_headroom_dimensions(
        &self,
        ledger: &Ledger,
        check_dollars: bool,
        request_usd: f64,
        request_tokens: u64,
    ) -> Result<bool> {
        let s = self.status(ledger)?;
        if self.stop_file.exists() {
            return Ok(false);
        }
        // A reservation fits if spend + reserved + request <= ceiling
        let fits_usd = !check_dollars
            || self.daily_budget_usd == 0.0
            || s.spend_today_usd + s.reserved_usd + request_usd <= self.daily_budget_usd;
        let fits_tokens = self.daily_output_tokens == 0
            || s.output_tokens_today + s.reserved_tokens + request_tokens
                <= self.daily_output_tokens;
        Ok(fits_usd && fits_tokens)
    }

    /// The lighter gate for paths that don't own a run lifecycle (MCP
    /// claims): refuses when the kill switch is thrown or a ceiling is
    /// already consumed by finished spend + live reservations.
    pub fn admit(&self, ledger: &Ledger) -> Result<()> {
        self.check_stop()?;
        let s = self.status(ledger)?;
        if self.daily_budget_usd > 0.0
            && s.spend_today_usd + s.reserved_usd >= self.daily_budget_usd
        {
            // "Claude-attributed": codex reports no cost and GLM's is
            // stripped as fiction, so this ceiling bounds only the spend the
            // ledger can see. The token ceiling is the cross-vendor backstop.
            anyhow::bail!(
                "governor: daily claude-attributed spend ceiling reached \
                 (${:.2} spent + ${:.2} reserved >= ${:.2}); raise \
                 daily_budget_usd in foreman.conf.mix (or override \
                 FOREMAN_DAILY_BUDGET_USD) or wait for UTC midnight",
                s.spend_today_usd,
                s.reserved_usd,
                self.daily_budget_usd
            );
        }
        if self.daily_output_tokens > 0
            && s.output_tokens_today + s.reserved_tokens >= self.daily_output_tokens
        {
            anyhow::bail!(
                "governor: daily output-token ceiling reached ({} spent + {} reserved \
                 >= {}); raise daily_output_tokens in foreman.conf.mix (or override \
                 FOREMAN_DAILY_OUTPUT_TOKENS) or wait for UTC midnight",
                s.output_tokens_today,
                s.reserved_tokens,
                self.daily_output_tokens
            );
        }
        Ok(())
    }

    pub fn stop(&self, reason: &str) -> Result<()> {
        std::fs::write(&self.stop_file, format!("{reason}\n"))
            .with_context(|| format!("writing {}", self.stop_file.display()))
    }

    pub fn resume(&self) -> Result<()> {
        match std::fs::remove_file(&self.stop_file) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("removing {}", self.stop_file.display())),
        }
    }
}

/// Sentinel error: the governor's authoritative reserve() call refused a
/// reservation for exceeding the daily ceiling. Distinct from IO or other
/// infrastructure errors — this is the same capacity issue as the preflight
/// (GovernorNoHeadroom in refinery), but at the binding gate where the hold
/// would have been taken. The refinery distinguishes this case via downcast
/// and treats it the same way: task restored to 'done', continue to next task.
#[derive(Debug)]
pub(crate) struct GovernorReservationRefused;

impl std::fmt::Display for GovernorReservationRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("governor reservation refused")
    }
}

impl std::error::Error for GovernorReservationRefused {}

/// Refuse a governed reservation up front for a driver that can never report
/// usage for a `--max-output-tokens` hold to bite against (checked BEFORE
/// [`Governor::reserve`], never after: a hold nothing enforces is a ceiling
/// that is fiction, worse than no hold — and a reservation this call refuses
/// is one `Governor::release` never has to clean up). Wrapped in
/// [`crate::driver::RungRefusal`] so dispatch applies the infrastructure
/// backoff without charging the task's quality ladder.
pub fn require_token_cap_enforcement(
    caps: &crate::executor::ExecutorCaps,
    agent: &str,
) -> Result<()> {
    if caps.enforces_token_cap {
        return Ok(());
    }
    let err = anyhow::anyhow!(
        "{agent} driver reports no usable token usage -- a governed \
         reservation against --max-output-tokens would never be enforced; \
         pass --no-governor to run it ungoverned, or dispatch a lane that \
         reports usage"
    );
    Err(err.context(crate::driver::RungRefusal))
}

/// A dollar budget on a task must never silently disappear on a lane whose
/// driver cannot meter or enforce dollars. The typed context makes dispatch
/// treat this as an infrastructure refusal with a short backoff.
pub fn require_task_budget_metering(
    kind: crate::executor::AgentKind,
    task_budget_usd: Option<f64>,
) -> Result<()> {
    if task_budget_usd.is_none() || kind.meters_dollars() {
        return Ok(());
    }
    let err = anyhow::anyhow!(
        "{} lane cannot meter or enforce task --budget in dollars; dispatch a dollar-metering lane",
        kind.as_str()
    );
    Err(err.context(crate::driver::RungRefusal))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::ExecutorCaps;

    #[test]
    fn refuses_a_driver_that_reports_no_usage() {
        let caps = ExecutorCaps {
            enforces_token_cap: false,
            ..Default::default()
        };
        let err = require_token_cap_enforcement(&caps, "hypothetical").expect_err(
            "a driver that never reports usage must not be reservable under the governor",
        );
        assert!(
            err.downcast_ref::<crate::driver::RungRefusal>().is_some(),
            "must carry the typed refusal marker: {err:#}"
        );
        assert!(
            format!("{err:#}").contains("hypothetical driver"),
            "names the refusing driver: {err:#}"
        );
    }

    #[test]
    fn admits_a_driver_that_reports_usage() {
        let caps = ExecutorCaps {
            enforces_token_cap: true,
            ..Default::default()
        };
        require_token_cap_enforcement(&caps, "claude")
            .expect("a driver whose usage the runner can act on must be reservable");
    }

    #[test]
    fn task_budget_replaces_policy_but_a_narrower_invocation_cap_wins() {
        assert_eq!(task_reservation_usd(5.0, None, Some(20.0)), 20.0);
        assert_eq!(task_reservation_usd(5.0, Some(0.75), Some(20.0)), 0.75);
        assert_eq!(task_reservation_usd(5.0, Some(25.0), Some(20.0)), 20.0);
        assert_eq!(task_reservation_usd(5.0, Some(0.75), Some(0.5)), 0.5);
        assert_eq!(task_reservation_usd(5.0, Some(0.75), None), 0.75);
        assert_eq!(task_reservation_usd(5.0, None, None), 5.0);
    }

    #[test]
    fn task_budget_refuses_non_metering_lanes() {
        require_task_budget_metering(crate::executor::AgentKind::Claude, Some(20.0)).unwrap();
        for kind in [
            crate::executor::AgentKind::Codex,
            crate::executor::AgentKind::Glm,
        ] {
            let err = require_task_budget_metering(kind, Some(20.0))
                .expect_err("a dollar budget cannot silently no-op");
            assert!(
                err.downcast_ref::<crate::driver::RungRefusal>().is_some(),
                "{err:#}"
            );
        }
    }
}
