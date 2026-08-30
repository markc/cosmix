//! Fleet policy loaded from an operator-selected `foreman.conf.mix`.
//!
//! The file is strict-data Mix, not executable Mix. Every command resolves a
//! snapshot at the start of its invocation. Environment variables remain a
//! one-shot escape hatch, with explicit precedence:
//!
//! `environment > foreman.conf.mix > compiled default`.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use cosmix_mix::value::Value;
use serde::Serialize;

use crate::executor::AgentKind;
use crate::governor::{
    DEFAULT_DAILY_BUDGET_USD, DEFAULT_DAILY_OUTPUT_TOKENS, DEFAULT_RESERVE_TOKENS,
    DEFAULT_RESERVE_USD,
};
use crate::ladder::{Ladder, Rung, parse_ladder};

pub const CONF_FILE: &str = "foreman.conf.mix";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Env,
    Conf,
    Project,
    Default,
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::Env => f.write_str("env"),
            Source::Conf => f.write_str("conf"),
            Source::Project => f.write_str("project"),
            Source::Default => f.write_str("default"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Sourced<T> {
    pub value: T,
    pub source: Source,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LadderPatience {
    pub default: u32,
    pub per_rung: BTreeMap<String, u32>,
}

impl std::fmt::Display for LadderPatience {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.per_rung.is_empty() {
            return write!(f, "{}", self.default);
        }
        write!(f, "default={} {:?}", self.default, self.per_rung)
    }
}

#[derive(Debug, Clone)]
pub struct FleetPolicy {
    pub path: PathBuf,
    pub file_found: bool,
    pub ladder: Sourced<Vec<Rung>>,
    pub start_rung: Sourced<usize>,
    pub ladder_patience: Sourced<LadderPatience>,
    /// Consecutive branch-contract or agent self-bounce dispositions allowed
    /// before the task is parked for an operator.
    pub branch_contract_limit: Sourced<u32>,
    pub daily_budget_usd: Sourced<f64>,
    pub daily_output_tokens: Sourced<u64>,
    /// Claude-side merge-authority model. The historical env name remains
    /// `FOREMAN_REVIEW_MODEL` for compatibility.
    pub review_model: Sourced<String>,
    pub codex_review_model: Sourced<String>,
    /// Maximum silence accepted from the streaming Claude review lane.
    pub review_stall_secs: Sourced<u64>,
    /// Maximum silence accepted from the Codex review lane, which can spend
    /// several minutes reasoning without emitting a JSON event.
    pub codex_review_stall_secs: Sourced<u64>,
    /// Default single-arm merge authority. Fleet evidence favours Codex, but
    /// this remains operator policy rather than a routing constant.
    pub review_primary: Sourced<AgentKind>,
    /// Second high-risk arm. Its model is resolved by family.
    pub review_secondary: Sourced<AgentKind>,
    /// Fixed one-arm merge-authority family, bypassing primary/secondary routing.
    pub review_override: Sourced<Option<AgentKind>>,
    /// Run both Claude and Codex arms for high-risk landings.
    pub two_arm_review: Sourced<bool>,
    /// Claude CLI binary invoked for merge-authority review sessions.
    /// Snapshotted once per [`Self::load_for_db`] call like every other
    /// review-lane setting: a review arm reads this from the policy it was
    /// handed, never live from the process environment, so a `FOREMAN_
    /// CLAUDE_BIN` mutation elsewhere in the process (another thread, a
    /// later command) cannot retarget a session that is already inflight.
    pub claude_bin: Sourced<String>,
    /// Codex CLI binary invoked for merge-authority review sessions. Same
    /// snapshot-once contract as [`Self::claude_bin`].
    pub codex_bin: Sourced<String>,
    /// Optional operator landing gate argv specification. This remains an
    /// environment-only local hook, but is resolved into the invocation
    /// snapshot instead of being read live after verification.
    pub landing_gate: Sourced<Option<String>>,
    /// Per-Claude-run dollar reservation used by governed review sessions.
    pub reserve_usd: Sourced<f64>,
    /// Per-review-run output-token reservation and enforcement cap.
    pub reserve_tokens: Sourced<u64>,
    /// Colon-separated sibling repositories refreshed before refining.
    /// Snapshotted with the rest of the local invocation environment.
    pub sibling_repos: Sourced<Option<String>>,
    /// Optional operator override for rust tier-1 feature coverage. The
    /// value is snapshotted once; verifier command construction never reads
    /// the process environment after an invocation has started.
    pub feature_sets: Sourced<Option<String>>,
    /// Optional crate/feature exclusions for auto-discovery, also captured
    /// in the invocation policy snapshot.
    pub feature_exclude: Sourced<Option<String>>,
    /// Filesystem lane serialising Cargo verifiers. An explicit environment
    /// path wins; project mode replaces only the host-wide default with its
    /// manifest-derived private lane.
    pub verify_lane: Sourced<PathBuf>,
    /// Maximum wait for a contended verifier lane. Acquisition polls so it
    /// can report the stamped holder rather than blocking invisibly in flock.
    pub verify_lane_wait_secs: Sourced<u64>,
    pub tier_timeout_secs: BTreeMap<u8, Sourced<u64>>,
    /// One command string per entry. Splitting into argv deliberately keeps
    /// the old `FOREMAN_TIER2_COMMANDS` whitespace semantics; no shell runs.
    pub tier2_commands: Sourced<Vec<String>>,
    /// Age and pressure policy consumed by the periodic Cargo-scratch sweep.
    pub scratch_terminal_age_hours: Sourced<u64>,
    pub scratch_pool: Sourced<Option<String>>,
    pub scratch_pressure_percent: Sourced<u8>,
    pub scratch_shared_max_gb: Sourced<u64>,
}

#[derive(Debug, Default)]
struct ConfValues {
    ladder: Option<Vec<Rung>>,
    start_rung: Option<usize>,
    ladder_patience: Option<LadderPatience>,
    branch_contract_limit: Option<u32>,
    daily_budget_usd: Option<f64>,
    daily_output_tokens: Option<u64>,
    review_model: Option<String>,
    codex_review_model: Option<String>,
    review_stall_secs: Option<u64>,
    codex_review_stall_secs: Option<u64>,
    review_primary: Option<AgentKind>,
    review_secondary: Option<AgentKind>,
    review_override: Option<AgentKind>,
    two_arm_review: Option<bool>,
    reserve_usd: Option<f64>,
    reserve_tokens: Option<u64>,
    tier_timeout_secs: BTreeMap<u8, u64>,
    tier2_commands: Option<Vec<String>>,
    scratch_terminal_age_hours: Option<u64>,
    scratch_pool: Option<String>,
    scratch_pressure_percent: Option<u8>,
    scratch_shared_max_gb: Option<u64>,
}

impl FleetPolicy {
    /// The legacy fleet-wide lane used when neither the environment nor a
    /// project manifest selects a private path.
    pub fn host_verify_lane_path() -> PathBuf {
        PathBuf::from("/tmp").join(format!(".foreman-verify-{}.lock", unsafe {
            libc::getuid()
        }))
    }

    /// Give project mode its per-manifest verifier lane without overriding
    /// an operator's explicit `FOREMAN_VERIFY_LANE` selection.
    pub fn scope_verify_lane_to_project(&mut self, root: &Path) {
        if self.verify_lane.source == Source::Default {
            self.verify_lane = Sourced {
                value: root.join("verify.lock"),
                source: Source::Project,
            };
        }
    }

    /// Resolve policy from an existing file beside the ledger, then
    /// `CONFIGURATION_DIRECTORY/foreman.conf.mix`.
    pub fn load_for_db(db: &Path) -> Result<Self> {
        Self::load_for_db_with(db, |key| std::env::var_os(key))
    }

    fn load_for_db_with(db: &Path, env: impl Fn(&str) -> Option<OsString>) -> Result<Self> {
        let beside_ledger = db
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(CONF_FILE);
        let default = if beside_ledger.try_exists()? {
            beside_ledger
        } else if let Some(dir) = env("CONFIGURATION_DIRECTORY") {
            let dir = PathBuf::from(dir);
            anyhow::ensure!(
                dir.is_absolute(),
                "CONFIGURATION_DIRECTORY must be an absolute path, got {}",
                dir.display()
            );
            dir.join(CONF_FILE)
        } else {
            beside_ledger
        };
        Self::load_with(default, env)
    }

    /// Apply invocation environment overrides without probing for a config
    /// file. Public verifier wrappers have no ledger from which to resolve a
    /// fleet config, but must retain the `FOREMAN_*` override contract.
    pub(crate) fn load_env_defaults() -> Result<Self> {
        Self::load_env_defaults_with(|key| std::env::var_os(key))
    }

    fn load_env_defaults_with(env: impl Fn(&str) -> Option<OsString>) -> Result<Self> {
        Self::resolve(PathBuf::from(CONF_FILE), false, ConfValues::default(), env)
    }

    /// Compiled policy only. This is for library callers that have no fleet
    /// home; CLI and daemon surfaces must use [`Self::load_for_db`].
    pub fn defaults() -> Self {
        Self::resolve(
            PathBuf::from(CONF_FILE),
            false,
            ConfValues::default(),
            |_| None,
        )
        .expect("compiled policy defaults are valid")
    }

    /// Load a named conf file without applying process-environment
    /// overrides. Useful to validate or seed a file before installing it;
    /// normal fleet commands use [`Self::load_for_db`].
    pub fn load_conf_file(path: &Path) -> Result<Self> {
        let (file_found, conf) = load_conf(path)?;
        Self::resolve(path.to_path_buf(), file_found, conf, |_| None)
    }

    /// Resolve a policy against an explicit environment provider. The
    /// provider is treated as the complete environment, not an overlay on
    /// the process environment, which lets library callers and tests build
    /// a stable invocation snapshot without mutating process-global state.
    pub fn load_with(
        default_path: PathBuf,
        env: impl Fn(&str) -> Option<OsString>,
    ) -> Result<Self> {
        let (file_found, conf) = load_conf(&default_path)?;
        Self::resolve(default_path, file_found, conf, env)
    }

    fn resolve(
        path: PathBuf,
        file_found: bool,
        conf: ConfValues,
        env: impl Fn(&str) -> Option<OsString>,
    ) -> Result<Self> {
        let default_ladder = Ladder::default();
        let ladder = match env_string(&env, "FOREMAN_LADDER")? {
            Some(spec) => Sourced {
                value: parse_ladder(&spec)
                    .with_context(|| format!("parsing FOREMAN_LADDER {spec:?}"))?,
                source: Source::Env,
            },
            None => sourced_or(conf.ladder, default_ladder.rungs),
        };

        let start_rung = match env_string(&env, "FOREMAN_START_RUNG")? {
            Some(value) => Sourced {
                value: parse_usize("FOREMAN_START_RUNG", &value)?,
                source: Source::Env,
            },
            None => sourced_or(conf.start_rung, default_ladder.start_rung),
        };
        anyhow::ensure!(
            start_rung.value < ladder.value.len(),
            "start_rung {} does not exist in the resolved {}-rung ladder",
            start_rung.value,
            ladder.value.len()
        );

        let ladder_patience = match env_string(&env, "FOREMAN_LADDER_PATIENCE")? {
            Some(value) => Sourced {
                value: LadderPatience {
                    default: parse_positive_u32("FOREMAN_LADDER_PATIENCE", &value)?,
                    per_rung: BTreeMap::new(),
                },
                source: Source::Env,
            },
            None => sourced_or(
                conf.ladder_patience,
                LadderPatience {
                    default: default_ladder.patience,
                    per_rung: BTreeMap::new(),
                },
            ),
        };

        let branch_contract_limit = match env_string(&env, "FOREMAN_BRANCH_CONTRACT_LIMIT")? {
            Some(value) => Sourced {
                value: parse_positive_u32("FOREMAN_BRANCH_CONTRACT_LIMIT", &value)?,
                source: Source::Env,
            },
            None => sourced_or(conf.branch_contract_limit, 3),
        };

        let daily_budget_usd = match env_string(&env, "FOREMAN_DAILY_BUDGET_USD")? {
            Some(value) => Sourced {
                value: parse_nonnegative_f64("FOREMAN_DAILY_BUDGET_USD", &value)?,
                source: Source::Env,
            },
            None => sourced_or(conf.daily_budget_usd, DEFAULT_DAILY_BUDGET_USD),
        };

        let daily_output_tokens = match env_string(&env, "FOREMAN_DAILY_OUTPUT_TOKENS")? {
            Some(value) => Sourced {
                value: parse_u64("FOREMAN_DAILY_OUTPUT_TOKENS", &value)?,
                source: Source::Env,
            },
            None => sourced_or(conf.daily_output_tokens, DEFAULT_DAILY_OUTPUT_TOKENS),
        };

        let review_model = match env_string(&env, "FOREMAN_REVIEW_MODEL")? {
            Some(value) => Sourced {
                value: nonempty("FOREMAN_REVIEW_MODEL", value)?,
                source: Source::Env,
            },
            None => sourced_or(conf.review_model, "opus".to_string()),
        };

        let codex_review_model = match env_string(&env, "FOREMAN_CODEX_REVIEW_MODEL")? {
            Some(value) => Sourced {
                value: nonempty("FOREMAN_CODEX_REVIEW_MODEL", value)?,
                source: Source::Env,
            },
            None => sourced_or(conf.codex_review_model, "gpt-5.6-sol".to_string()),
        };

        let review_stall_secs = match env_string(&env, "FOREMAN_REVIEW_STALL_SECS")? {
            Some(value) => Sourced {
                value: parse_positive_u64("FOREMAN_REVIEW_STALL_SECS", &value)?,
                source: Source::Env,
            },
            None => sourced_or(conf.review_stall_secs, 300),
        };

        let codex_review_stall_secs = match env_string(&env, "FOREMAN_CODEX_REVIEW_STALL_SECS")? {
            Some(value) => Sourced {
                value: parse_positive_u64("FOREMAN_CODEX_REVIEW_STALL_SECS", &value)?,
                source: Source::Env,
            },
            None => sourced_or(conf.codex_review_stall_secs, 900),
        };

        let review_primary = match env_string(&env, "FOREMAN_REVIEW_PRIMARY")? {
            Some(value) => Sourced {
                value: parse_reviewer("FOREMAN_REVIEW_PRIMARY", &value)?,
                source: Source::Env,
            },
            None => sourced_or(conf.review_primary, AgentKind::Codex),
        };

        let review_secondary = match env_string(&env, "FOREMAN_REVIEW_SECONDARY")? {
            Some(value) => Sourced {
                value: parse_reviewer("FOREMAN_REVIEW_SECONDARY", &value)?,
                source: Source::Env,
            },
            None => sourced_or(conf.review_secondary, AgentKind::Claude),
        };
        anyhow::ensure!(
            review_primary.value != review_secondary.value,
            "review_primary and review_secondary must name different reviewer families"
        );

        let review_override = match env_string(&env, "FOREMAN_REVIEW_OVERRIDE")? {
            Some(value) => Sourced {
                value: Some(parse_reviewer("FOREMAN_REVIEW_OVERRIDE", &value)?),
                source: Source::Env,
            },
            None => match conf.review_override {
                Some(value) => sourced(Some(value), Source::Conf),
                None => sourced(None, Source::Default),
            },
        };

        let two_arm_review = match env_string(&env, "FOREMAN_TWO_ARM_REVIEW")? {
            Some(value) => Sourced {
                value: parse_bool("FOREMAN_TWO_ARM_REVIEW", &value)?,
                source: Source::Env,
            },
            None => sourced_or(conf.two_arm_review, true),
        };

        // Env-only (no conf-file key): these name a local binary, not a
        // fleet policy value, so there is nothing to persist alongside the
        // review model/ladder settings above.
        let claude_bin = match env_string(&env, "FOREMAN_CLAUDE_BIN")? {
            Some(value) => Sourced {
                value: nonempty("FOREMAN_CLAUDE_BIN", value)?,
                source: Source::Env,
            },
            None => sourced("claude".to_string(), Source::Default),
        };
        let codex_bin = match env_string(&env, "FOREMAN_CODEX_BIN")? {
            Some(value) => Sourced {
                value: nonempty("FOREMAN_CODEX_BIN", value)?,
                source: Source::Env,
            },
            None => sourced("codex".to_string(), Source::Default),
        };
        let landing_gate = match env_string(&env, "FOREMAN_LANDING_GATE")? {
            Some(value) => sourced(Some(value), Source::Env),
            None => sourced(None, Source::Default),
        };
        let reserve_usd = match env_string(&env, "FOREMAN_RESERVE_USD")? {
            Some(value) => Sourced {
                value: parse_nonnegative_f64("FOREMAN_RESERVE_USD", &value)?,
                source: Source::Env,
            },
            None => sourced_or(conf.reserve_usd, DEFAULT_RESERVE_USD),
        };
        let reserve_tokens = match env_string(&env, "FOREMAN_RESERVE_TOKENS")? {
            Some(value) => Sourced {
                value: parse_u64("FOREMAN_RESERVE_TOKENS", &value)?,
                source: Source::Env,
            },
            None => sourced_or(conf.reserve_tokens, DEFAULT_RESERVE_TOKENS),
        };
        let sibling_repos = match env_string(&env, crate::refinery::SIBLING_REPOS_ENV)? {
            Some(value) => sourced(Some(value), Source::Env),
            None => sourced(None, Source::Default),
        };
        let feature_sets = match env_string(&env, "FOREMAN_FEATURE_SETS")? {
            Some(value) => sourced(Some(value), Source::Env),
            None => sourced(None, Source::Default),
        };
        let feature_exclude = match env_string(&env, "FOREMAN_FEATURE_EXCLUDE")? {
            Some(value) => sourced(Some(value), Source::Env),
            None => sourced(None, Source::Default),
        };

        let verify_lane = match env("FOREMAN_VERIFY_LANE") {
            Some(value) if value.is_empty() => {
                anyhow::bail!("FOREMAN_VERIFY_LANE must not be empty")
            }
            Some(value) => {
                let value = PathBuf::from(value);
                anyhow::ensure!(
                    value.is_absolute(),
                    "FOREMAN_VERIFY_LANE must be an absolute path, got {}",
                    value.display()
                );
                Sourced {
                    value,
                    source: Source::Env,
                }
            }
            None => sourced(Self::host_verify_lane_path(), Source::Default),
        };
        let verify_lane_wait_secs = match env_string(&env, "FOREMAN_VERIFY_LANE_WAIT_SECS")? {
            Some(value) => Sourced {
                value: parse_u64("FOREMAN_VERIFY_LANE_WAIT_SECS", &value)?,
                source: Source::Env,
            },
            None => sourced(900, Source::Default),
        };

        let mut tier_timeout_secs = BTreeMap::from([
            (0, sourced(600, Source::Default)),
            (1, sourced(1800, Source::Default)),
            (2, sourced(3600, Source::Default)),
        ]);
        for (tier, secs) in conf.tier_timeout_secs {
            tier_timeout_secs.insert(tier, sourced(secs, Source::Conf));
        }
        for tier in 0..=2 {
            let key = format!("FOREMAN_TIER{tier}_TIMEOUT_SECS");
            if let Some(value) = env_string(&env, &key)? {
                let secs = parse_positive_u64(&key, &value)?;
                tier_timeout_secs.insert(tier, sourced(secs, Source::Env));
            }
        }

        let tier2_commands = match env_string(&env, "FOREMAN_TIER2_COMMANDS")? {
            Some(value) => Sourced {
                value: value
                    .split(";;")
                    .map(str::trim)
                    .filter(|part| !part.is_empty())
                    .map(str::to_string)
                    .collect(),
                source: Source::Env,
            },
            None => sourced_or(conf.tier2_commands, Vec::new()),
        };

        let scratch_terminal_age_hours =
            match env_string(&env, "FOREMAN_SCRATCH_TERMINAL_AGE_HOURS")? {
                Some(value) => Sourced {
                    value: parse_u64("FOREMAN_SCRATCH_TERMINAL_AGE_HOURS", &value)?,
                    source: Source::Env,
                },
                None => sourced_or(
                    conf.scratch_terminal_age_hours,
                    crate::scratch::DEFAULT_TERMINAL_AGE_HOURS,
                ),
            };
        let scratch_pool = match env_string(&env, "FOREMAN_SCRATCH_POOL")? {
            Some(value) => sourced(Some(nonempty("FOREMAN_SCRATCH_POOL", value)?), Source::Env),
            None => match conf.scratch_pool {
                Some(value) => sourced(Some(value), Source::Conf),
                None => sourced(None, Source::Default),
            },
        };
        let scratch_pressure_percent = match env_string(&env, "FOREMAN_SCRATCH_PRESSURE_PERCENT")? {
            Some(value) => Sourced {
                value: parse_percent("FOREMAN_SCRATCH_PRESSURE_PERCENT", &value)?,
                source: Source::Env,
            },
            None => sourced_or(
                conf.scratch_pressure_percent,
                crate::scratch::DEFAULT_PRESSURE_PERCENT,
            ),
        };
        let scratch_shared_max_gb = match env_string(&env, "FOREMAN_SCRATCH_SHARED_MAX_GB")? {
            Some(value) => Sourced {
                value: parse_positive_u64("FOREMAN_SCRATCH_SHARED_MAX_GB", &value)?,
                source: Source::Env,
            },
            None => sourced_or(
                conf.scratch_shared_max_gb,
                crate::scratch::DEFAULT_SHARED_MAX_GB,
            ),
        };

        Ok(FleetPolicy {
            path,
            file_found,
            ladder,
            start_rung,
            ladder_patience,
            branch_contract_limit,
            daily_budget_usd,
            daily_output_tokens,
            review_model,
            codex_review_model,
            review_stall_secs,
            codex_review_stall_secs,
            review_primary,
            review_secondary,
            review_override,
            two_arm_review,
            claude_bin,
            codex_bin,
            landing_gate,
            reserve_usd,
            reserve_tokens,
            sibling_repos,
            feature_sets,
            feature_exclude,
            verify_lane,
            verify_lane_wait_secs,
            tier_timeout_secs,
            tier2_commands,
            scratch_terminal_age_hours,
            scratch_pool,
            scratch_pressure_percent,
            scratch_shared_max_gb,
        })
    }

    pub fn ladder(&self) -> Ladder {
        Ladder {
            rungs: self.ladder.value.clone(),
            start_rung: self.start_rung.value,
            patience: self.ladder_patience.value.default,
            per_rung_patience: self.ladder_patience.value.per_rung.clone(),
        }
    }

    pub fn tier_timeout(&self, tier: u8) -> Result<std::time::Duration> {
        self.tier_timeout_secs
            .get(&tier)
            .map(|entry| std::time::Duration::from_secs(entry.value))
            .with_context(|| format!("no timeout policy for verifier tier {tier}"))
    }

    pub fn tier2_argv(&self) -> Vec<Vec<String>> {
        self.tier2_commands
            .value
            .iter()
            .map(|command| command.split_whitespace().map(str::to_string).collect())
            .collect()
    }

    pub fn json(&self) -> serde_json::Value {
        let ladder: Vec<String> = self.ladder.value.iter().map(ToString::to_string).collect();
        let timeout_values: BTreeMap<String, u64> = self
            .tier_timeout_secs
            .iter()
            .map(|(tier, value)| (tier.to_string(), value.value))
            .collect();
        let timeout_sources: BTreeMap<String, Source> = self
            .tier_timeout_secs
            .iter()
            .map(|(tier, value)| (tier.to_string(), value.source))
            .collect();
        serde_json::json!({
            "conf_file": {
                "path": self.path,
                "found": self.file_found,
            },
            "ladder": {"value": ladder, "source": self.ladder.source},
            "start_rung": {"value": self.start_rung.value, "source": self.start_rung.source},
            "ladder_patience": {"value": self.ladder_patience.value, "source": self.ladder_patience.source},
            "branch_contract_limit": {"value": self.branch_contract_limit.value, "source": self.branch_contract_limit.source},
            "daily_budget_usd": {"value": self.daily_budget_usd.value, "source": self.daily_budget_usd.source},
            "daily_output_tokens": {"value": self.daily_output_tokens.value, "source": self.daily_output_tokens.source},
            "review_model": {"value": self.review_model.value, "source": self.review_model.source},
            "codex_review_model": {"value": self.codex_review_model.value, "source": self.codex_review_model.source},
            "review_stall_secs": {"value": self.review_stall_secs.value, "source": self.review_stall_secs.source},
            "codex_review_stall_secs": {"value": self.codex_review_stall_secs.value, "source": self.codex_review_stall_secs.source},
            "review_primary": {"value": self.review_primary.value, "source": self.review_primary.source},
            "review_secondary": {"value": self.review_secondary.value, "source": self.review_secondary.source},
            "review_override": {"value": self.review_override.value, "source": self.review_override.source},
            "two_arm_review": {"value": self.two_arm_review.value, "source": self.two_arm_review.source},
            "claude_bin": {"value": self.claude_bin.value, "source": self.claude_bin.source},
            "codex_bin": {"value": self.codex_bin.value, "source": self.codex_bin.source},
            "landing_gate": {"value": self.landing_gate.value, "source": self.landing_gate.source},
            "reserve_usd": {"value": self.reserve_usd.value, "source": self.reserve_usd.source},
            "reserve_tokens": {"value": self.reserve_tokens.value, "source": self.reserve_tokens.source},
            "sibling_repos": {"value": self.sibling_repos.value, "source": self.sibling_repos.source},
            "feature_sets": {"value": self.feature_sets.value, "source": self.feature_sets.source},
            "feature_exclude": {"value": self.feature_exclude.value, "source": self.feature_exclude.source},
            "verify_lane": {"value": self.verify_lane.value, "source": self.verify_lane.source},
            "verify_lane_wait_secs": {"value": self.verify_lane_wait_secs.value, "source": self.verify_lane_wait_secs.source},
            "tier_timeout_secs": {"value": timeout_values, "source": timeout_sources},
            "tier2_commands": {"value": self.tier2_commands.value, "source": self.tier2_commands.source},
            "scratch_terminal_age_hours": {"value": self.scratch_terminal_age_hours.value, "source": self.scratch_terminal_age_hours.source},
            "scratch_pool": {"value": self.scratch_pool.value, "source": self.scratch_pool.source},
            "scratch_pressure_percent": {"value": self.scratch_pressure_percent.value, "source": self.scratch_pressure_percent.source},
            "scratch_shared_max_gb": {"value": self.scratch_shared_max_gb.value, "source": self.scratch_shared_max_gb.source},
        })
    }

    pub fn render_text(&self) -> Vec<String> {
        let state = if self.file_found { "loaded" } else { "missing" };
        let mut lines = vec![format!("conf file: {} ({state})", self.path.display())];
        lines.push(format!(
            "ladder: [{}] (source: {})",
            self.ladder
                .value
                .iter()
                .map(|r| format!("\"{r}\""))
                .collect::<Vec<_>>()
                .join(", "),
            self.ladder.source
        ));
        lines.push(format!(
            "start_rung: {} (source: {})",
            self.start_rung.value, self.start_rung.source
        ));
        lines.push(format!(
            "ladder_patience: {} (source: {})",
            self.ladder_patience.value, self.ladder_patience.source
        ));
        lines.push(format!(
            "branch_contract_limit: {} (source: {})",
            self.branch_contract_limit.value, self.branch_contract_limit.source
        ));
        lines.push(format!(
            "daily_budget_usd: {} (source: {})",
            self.daily_budget_usd.value, self.daily_budget_usd.source
        ));
        lines.push(format!(
            "daily_output_tokens: {} (source: {})",
            self.daily_output_tokens.value, self.daily_output_tokens.source
        ));
        lines.push(format!(
            "review_model: {:?} (source: {})",
            self.review_model.value, self.review_model.source
        ));
        lines.push(format!(
            "codex_review_model: {:?} (source: {})",
            self.codex_review_model.value, self.codex_review_model.source
        ));
        lines.push(format!(
            "review_stall_secs: {} (source: {})",
            self.review_stall_secs.value, self.review_stall_secs.source
        ));
        lines.push(format!(
            "codex_review_stall_secs: {} (source: {})",
            self.codex_review_stall_secs.value, self.codex_review_stall_secs.source
        ));
        lines.push(format!(
            "review_primary: {:?} (source: {})",
            self.review_primary.value.as_str(),
            self.review_primary.source
        ));
        lines.push(format!(
            "review_secondary: {:?} (source: {})",
            self.review_secondary.value.as_str(),
            self.review_secondary.source
        ));
        lines.push(format!(
            "review_override: {:?} (source: {})",
            self.review_override.value.map(|kind| kind.as_str()),
            self.review_override.source
        ));
        lines.push(format!(
            "two_arm_review: {} (source: {})",
            self.two_arm_review.value, self.two_arm_review.source
        ));
        lines.push(format!(
            "claude_bin: {:?} (source: {})",
            self.claude_bin.value, self.claude_bin.source
        ));
        lines.push(format!(
            "codex_bin: {:?} (source: {})",
            self.codex_bin.value, self.codex_bin.source
        ));
        lines.push(format!(
            "landing_gate: {:?} (source: {})",
            self.landing_gate.value, self.landing_gate.source
        ));
        lines.push(format!(
            "reserve_usd: {} (source: {})",
            self.reserve_usd.value, self.reserve_usd.source
        ));
        lines.push(format!(
            "reserve_tokens: {} (source: {})",
            self.reserve_tokens.value, self.reserve_tokens.source
        ));
        lines.push(format!(
            "sibling_repos: {:?} (source: {})",
            self.sibling_repos.value, self.sibling_repos.source
        ));
        lines.push(format!(
            "feature_sets: {:?} (source: {})",
            self.feature_sets.value, self.feature_sets.source
        ));
        lines.push(format!(
            "feature_exclude: {:?} (source: {})",
            self.feature_exclude.value, self.feature_exclude.source
        ));
        lines.push(format!(
            "verify_lane: {} (source: {})",
            self.verify_lane.value.display(),
            self.verify_lane.source
        ));
        lines.push(format!(
            "verify_lane_wait_secs: {} (source: {})",
            self.verify_lane_wait_secs.value, self.verify_lane_wait_secs.source
        ));
        lines.push("tier_timeout_secs:".to_string());
        for (tier, entry) in &self.tier_timeout_secs {
            lines.push(format!(
                "  {tier}: {} (source: {})",
                entry.value, entry.source
            ));
        }
        lines.push(format!(
            "tier2_commands: {:?} (source: {})",
            self.tier2_commands.value, self.tier2_commands.source
        ));
        lines.push(format!(
            "scratch_terminal_age_hours: {} (source: {})",
            self.scratch_terminal_age_hours.value, self.scratch_terminal_age_hours.source
        ));
        lines.push(format!(
            "scratch_pool: {:?} (source: {})",
            self.scratch_pool.value, self.scratch_pool.source
        ));
        lines.push(format!(
            "scratch_pressure_percent: {} (source: {})",
            self.scratch_pressure_percent.value, self.scratch_pressure_percent.source
        ));
        lines.push(format!(
            "scratch_shared_max_gb: {} (source: {})",
            self.scratch_shared_max_gb.value, self.scratch_shared_max_gb.source
        ));
        lines
    }
}

fn sourced_or<T>(configured: Option<T>, default: T) -> Sourced<T> {
    match configured {
        Some(value) => sourced(value, Source::Conf),
        None => sourced(default, Source::Default),
    }
}

fn sourced<T>(value: T, source: Source) -> Sourced<T> {
    Sourced { value, source }
}

fn env_string(env: &impl Fn(&str) -> Option<OsString>, key: &str) -> Result<Option<String>> {
    env(key)
        .map(|value| {
            value
                .into_string()
                .map_err(|_| anyhow::anyhow!("{key} is not valid unicode"))
        })
        .transpose()
}

fn load_conf(path: &Path) -> Result<(bool, ConfValues)> {
    match std::fs::metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((false, ConfValues::default()));
        }
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    }
    let value = cosmix_mix::parse_data_file(path)
        .map_err(|error| anyhow::anyhow!("parsing {}: {error}", path.display()))?;
    let Value::Map(ref entries) = value else {
        anyhow::bail!("{}: top level must be a map", path.display());
    };
    let mut out = ConfValues::default();
    for (key, value) in entries.iter() {
        match key.as_str() {
            "ladder" => {
                let specs = string_list(key, value)?;
                out.ladder =
                    Some(parse_ladder(&specs.join(",")).context("invalid config key `ladder`")?);
            }
            "start_rung" => {
                out.start_rung = Some(usize_value(key, value)?);
            }
            "ladder_patience" => {
                out.ladder_patience = Some(parse_ladder_patience(key, value)?);
            }
            "branch_contract_limit" => {
                out.branch_contract_limit = Some(positive_u32_value(key, value)?);
            }
            "daily_budget_usd" => {
                out.daily_budget_usd = Some(nonnegative_number(key, value)?);
            }
            "daily_output_tokens" => {
                out.daily_output_tokens = Some(u64_value(key, value)?);
            }
            "review_model" => {
                out.review_model = Some(nonempty(key, string_value(key, value)?.to_string())?);
            }
            "codex_review_model" => {
                out.codex_review_model =
                    Some(nonempty(key, string_value(key, value)?.to_string())?);
            }
            "review_stall_secs" => {
                out.review_stall_secs = Some(positive_u64_value(key, value)?);
            }
            "codex_review_stall_secs" => {
                out.codex_review_stall_secs = Some(positive_u64_value(key, value)?);
            }
            "review_primary" => {
                out.review_primary = Some(parse_reviewer(key, string_value(key, value)?)?);
            }
            "review_secondary" => {
                out.review_secondary = Some(parse_reviewer(key, string_value(key, value)?)?);
            }
            "review_override" => {
                out.review_override = Some(parse_reviewer(key, string_value(key, value)?)?);
            }
            "two_arm_review" => {
                out.two_arm_review = Some(bool_value(key, value)?);
            }
            "reserve_usd" => {
                out.reserve_usd = Some(nonnegative_number(key, value)?);
            }
            "reserve_tokens" => {
                out.reserve_tokens = Some(u64_value(key, value)?);
            }
            "tier_timeout_secs" => {
                let Value::Map(timeouts) = value else {
                    anyhow::bail!("config key `tier_timeout_secs` must be a map");
                };
                for (tier, value) in timeouts.iter() {
                    let parsed: u8 = tier.parse().with_context(|| {
                        format!("unknown config key `tier_timeout_secs.{tier}`")
                    })?;
                    anyhow::ensure!(parsed <= 2, "unknown config key `tier_timeout_secs.{tier}`");
                    out.tier_timeout_secs.insert(
                        parsed,
                        positive_u64_value(&format!("tier_timeout_secs.{tier}"), value)?,
                    );
                }
            }
            "tier2_commands" => {
                let commands = string_list(key, value)?;
                for command in &commands {
                    anyhow::ensure!(
                        !command.trim().is_empty(),
                        "config key `tier2_commands` contains an empty command"
                    );
                }
                out.tier2_commands = Some(commands);
            }
            "scratch_terminal_age_hours" => {
                out.scratch_terminal_age_hours = Some(u64_value(key, value)?);
            }
            "scratch_pool" => {
                out.scratch_pool = Some(nonempty(key, string_value(key, value)?.to_string())?);
            }
            "scratch_pressure_percent" => {
                let percent = u8::try_from(u64_value(key, value)?)
                    .with_context(|| format!("config key `{key}` is larger than u8"))?;
                anyhow::ensure!(
                    (1..=100).contains(&percent),
                    "config key `{key}` must be in 1..=100"
                );
                out.scratch_pressure_percent = Some(percent);
            }
            "scratch_shared_max_gb" => {
                out.scratch_shared_max_gb = Some(positive_u64_value(key, value)?);
            }
            _ => anyhow::bail!("unknown config key `{key}`"),
        }
    }
    Ok((true, out))
}

/// Shared with [`crate::manifest`], which parses the same strict-data Mix
/// format for a different file — one scalar-extraction vocabulary, not two
/// that can drift apart.
pub(crate) fn string_value<'a>(key: &str, value: &'a Value) -> Result<&'a str> {
    match value {
        Value::String(value) => Ok(value),
        _ => anyhow::bail!("config key `{key}` must be a string"),
    }
}

pub(crate) fn bool_value(key: &str, value: &Value) -> Result<bool> {
    match value {
        Value::Bool(value) => Ok(*value),
        _ => anyhow::bail!("config key `{key}` must be a bool"),
    }
}

fn parse_reviewer(key: &str, value: &str) -> Result<AgentKind> {
    let reviewer = value
        .parse::<AgentKind>()
        .map_err(anyhow::Error::msg)
        .with_context(|| format!("parsing {key}"))?;
    anyhow::ensure!(
        reviewer != AgentKind::Glm,
        "{key} cannot be glm: GLM is never merge authority"
    );
    Ok(reviewer)
}

pub(crate) fn string_list(key: &str, value: &Value) -> Result<Vec<String>> {
    let Value::List(values) = value else {
        anyhow::bail!("config key `{key}` must be a list of strings");
    };
    values
        .iter()
        .map(|value| string_value(key, value).map(str::to_string))
        .collect()
}

fn nonnegative_number(key: &str, value: &Value) -> Result<f64> {
    match value {
        Value::Number(value) if value.is_finite() && *value >= 0.0 => Ok(*value),
        _ => anyhow::bail!("config key `{key}` must be a finite non-negative number"),
    }
}

pub(crate) fn u64_value(key: &str, value: &Value) -> Result<u64> {
    let number = nonnegative_number(key, value)?;
    anyhow::ensure!(
        number.fract() == 0.0 && number <= 9_007_199_254_740_992.0,
        "config key `{key}` must be a whole number no larger than 2^53"
    );
    Ok(number as u64)
}

fn usize_value(key: &str, value: &Value) -> Result<usize> {
    usize::try_from(u64_value(key, value)?)
        .with_context(|| format!("config key `{key}` is larger than usize"))
}

fn positive_u64_value(key: &str, value: &Value) -> Result<u64> {
    let value = u64_value(key, value)?;
    anyhow::ensure!(value > 0, "config key `{key}` must be greater than zero");
    Ok(value)
}

fn positive_u32_value(key: &str, value: &Value) -> Result<u32> {
    let value = positive_u64_value(key, value)?;
    u32::try_from(value).with_context(|| format!("config key `{key}` is larger than u32"))
}

fn parse_ladder_patience(key: &str, value: &Value) -> Result<LadderPatience> {
    match value {
        Value::Number(_) => Ok(LadderPatience {
            default: positive_u32_value(key, value)?,
            per_rung: BTreeMap::new(),
        }),
        Value::Map(entries) => {
            let mut default = Ladder::default().patience;
            let mut per_rung = BTreeMap::new();
            for (rung, value) in entries.iter() {
                let patience = positive_u32_value(&format!("{key}.{rung}"), value)?;
                if rung == "default" {
                    default = patience;
                } else {
                    anyhow::ensure!(
                        rung.parse::<AgentKind>().is_ok()
                            || parse_ladder(rung).is_ok_and(|v| v.len() == 1),
                        "config key `{key}.{rung}` is not an agent or full rung"
                    );
                    per_rung.insert(rung.clone(), patience);
                }
            }
            Ok(LadderPatience { default, per_rung })
        }
        _ => anyhow::bail!("config key `{key}` must be a positive integer or a rung map"),
    }
}

pub(crate) fn nonempty(key: &str, value: String) -> Result<String> {
    anyhow::ensure!(!value.trim().is_empty(), "{key} must not be empty");
    Ok(value)
}

fn parse_nonnegative_f64(key: &str, value: &str) -> Result<f64> {
    let parsed: f64 = value
        .parse()
        .with_context(|| format!("parsing {key} {value:?}"))?;
    anyhow::ensure!(
        parsed.is_finite() && parsed >= 0.0,
        "{key} must be a finite non-negative number"
    );
    Ok(parsed)
}

fn parse_u64(key: &str, value: &str) -> Result<u64> {
    value
        .parse()
        .with_context(|| format!("parsing {key} {value:?}"))
}

fn parse_usize(key: &str, value: &str) -> Result<usize> {
    usize::try_from(parse_u64(key, value)?).with_context(|| format!("{key} is larger than usize"))
}

fn parse_positive_u64(key: &str, value: &str) -> Result<u64> {
    let parsed = parse_u64(key, value)?;
    anyhow::ensure!(parsed > 0, "{key} must be greater than zero");
    Ok(parsed)
}

fn parse_percent(key: &str, value: &str) -> Result<u8> {
    let parsed: u8 = value
        .parse()
        .with_context(|| format!("parsing {key} {value:?}"))?;
    anyhow::ensure!((1..=100).contains(&parsed), "{key} must be in 1..=100");
    Ok(parsed)
}

fn parse_positive_u32(key: &str, value: &str) -> Result<u32> {
    let parsed: u32 = value
        .parse()
        .with_context(|| format!("parsing {key} {value:?}"))?;
    anyhow::ensure!(parsed > 0, "{key} must be greater than zero");
    Ok(parsed)
}

fn parse_bool(key: &str, value: &str) -> Result<bool> {
    match value {
        "1" => Ok(true),
        "0" => Ok(false),
        value if value.eq_ignore_ascii_case("true") => Ok(true),
        value if value.eq_ignore_ascii_case("false") => Ok(false),
        _ => anyhow::bail!("{key} must be 0|1|false|true, got {value:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn load(source: Option<&str>, env: &[(&str, &str)]) -> Result<FleetPolicy> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join(CONF_FILE);
        if let Some(source) = source {
            std::fs::write(&path, source)?;
        }
        let env: HashMap<String, OsString> = env
            .iter()
            .map(|(key, value)| (key.to_string(), OsString::from(value)))
            .collect();
        FleetPolicy::load_with(path, |key| env.get(key).cloned())
    }

    #[test]
    fn resolves_env_over_conf_over_default_with_sources() {
        let policy = load(
            Some(
                "ladder: [\"glm\", \"claude:sonnet\"]\n\
                 start_rung: 1\n\
                 daily_budget_usd: 300\n\
                 review_model: \"sonnet\"\n",
            ),
            &[
                ("FOREMAN_LADDER", "codex,claude:opus"),
                ("FOREMAN_START_RUNG", "0"),
                ("FOREMAN_DAILY_BUDGET_USD", "400"),
            ],
        )
        .unwrap();
        assert_eq!(policy.ladder.source, Source::Env);
        assert_eq!(policy.ladder.value[0].to_string(), "codex");
        assert_eq!(policy.start_rung.value, 0);
        assert_eq!(policy.start_rung.source, Source::Env);
        assert_eq!(policy.daily_budget_usd.value, 400.0);
        assert_eq!(policy.daily_budget_usd.source, Source::Env);
        assert_eq!(policy.review_model.value, "sonnet");
        assert_eq!(policy.review_model.source, Source::Conf);
        assert_eq!(policy.ladder_patience.value.default, 2);
        assert_eq!(policy.ladder_patience.source, Source::Default);
    }

    #[test]
    fn invalid_conf_ladder_names_the_key() {
        let error = load(Some("ladder: [\"invalid\"]\n"), &[]).unwrap_err();
        assert!(format!("{error:#}").contains("ladder"), "{error:#}");
    }

    #[test]
    fn retired_acp_conf_ladder_is_refused_at_load() {
        let error = load(Some("ladder: [\"acp\"]\n"), &[]).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("config key `ladder`"), "{message}");
        assert!(
            message.contains("acp lane is retired") && message.contains("use claude instead"),
            "{message}"
        );
    }

    #[test]
    fn missing_file_reports_defaults() {
        let policy = load(None, &[]).unwrap();
        assert!(!policy.file_found);
        assert_eq!(policy.ladder.source, Source::Default);
        assert_eq!(policy.start_rung.value, 0);
        assert_eq!(policy.start_rung.source, Source::Default);
        assert_eq!(policy.daily_budget_usd.source, Source::Default);
        assert_eq!(policy.review_model.value, "opus");
        assert_eq!(policy.codex_review_model.value, "gpt-5.6-sol");
        assert_eq!(
            policy.scratch_terminal_age_hours.value,
            crate::scratch::DEFAULT_TERMINAL_AGE_HOURS
        );
        assert_eq!(policy.scratch_pool.value, None);
        assert_eq!(
            policy.scratch_pressure_percent.value,
            crate::scratch::DEFAULT_PRESSURE_PERCENT
        );
        assert_eq!(
            policy.scratch_shared_max_gb.value,
            crate::scratch::DEFAULT_SHARED_MAX_GB
        );
        assert_eq!(policy.review_stall_secs.value, 300);
        assert_eq!(policy.codex_review_stall_secs.value, 900);
        assert_eq!(policy.review_primary.value, AgentKind::Codex);
        assert_eq!(policy.review_secondary.value, AgentKind::Claude);
        assert_eq!(policy.review_override.value, None);
        assert!(policy.two_arm_review.value);
        assert_eq!(policy.json()["ladder"]["source"], "default");
        assert_eq!(policy.json()["start_rung"]["source"], "default");
    }

    #[test]
    fn env_only_policy_keeps_verifier_overrides_without_reading_cwd_conf() {
        let policy = FleetPolicy::load_env_defaults_with(|key| match key {
            "FOREMAN_TIER1_TIMEOUT_SECS" => Some(OsString::from("4321")),
            "FOREMAN_FEATURE_SETS" => Some(OsString::from("fixture:feature")),
            "FOREMAN_VERIFY_LANE" => Some(OsString::from("/tmp/private-verify.lock")),
            "FOREMAN_VERIFY_LANE_WAIT_SECS" => Some(OsString::from("30")),
            _ => None,
        })
        .unwrap();

        assert!(!policy.file_found);
        assert_eq!(policy.tier_timeout_secs[&1].value, 4321);
        assert_eq!(policy.tier_timeout_secs[&1].source, Source::Env);
        assert_eq!(
            policy.feature_sets.value.as_deref(),
            Some("fixture:feature")
        );
        assert_eq!(policy.feature_sets.source, Source::Env);
        assert_eq!(
            policy.verify_lane.value,
            Path::new("/tmp/private-verify.lock")
        );
        assert_eq!(policy.verify_lane.source, Source::Env);
        assert_eq!(policy.verify_lane_wait_secs.value, 30);
        assert_eq!(policy.verify_lane_wait_secs.source, Source::Env);
    }

    #[test]
    fn project_scopes_only_the_default_verify_lane() {
        let root = Path::new("/tmp/.foreman-project-demo");
        let mut default = FleetPolicy::defaults();
        default.scope_verify_lane_to_project(root);
        assert_eq!(default.verify_lane.value, root.join("verify.lock"));
        assert_eq!(default.verify_lane.source, Source::Project);

        let mut explicit = FleetPolicy::load_env_defaults_with(|key| {
            (key == "FOREMAN_VERIFY_LANE").then(|| OsString::from("/tmp/operator.lock"))
        })
        .unwrap();
        explicit.scope_verify_lane_to_project(root);
        assert_eq!(explicit.verify_lane.value, Path::new("/tmp/operator.lock"));
        assert_eq!(explicit.verify_lane.source, Source::Env);
    }

    #[test]
    fn review_routing_policy_loads_from_conf() {
        let policy = load(
            Some(
                "review_model: \"claude-conf\"\n\
                 codex_review_model: \"codex-conf\"\n\
                 review_primary: \"claude\"\n\
                 review_secondary: \"codex\"\n\
                 review_override: \"codex\"\n\
                 two_arm_review: true\n",
            ),
            &[],
        )
        .unwrap();
        assert_eq!(policy.review_model.value, "claude-conf");
        assert_eq!(policy.codex_review_model.value, "codex-conf");
        assert_eq!(policy.review_primary.value, AgentKind::Claude);
        assert_eq!(policy.review_secondary.value, AgentKind::Codex);
        assert_eq!(policy.review_override.value, Some(AgentKind::Codex));
        assert!(policy.two_arm_review.value);
        assert_eq!(policy.two_arm_review.source, Source::Conf);
    }

    #[test]
    fn review_stall_budgets_load_per_lane_from_conf() {
        let policy = load(
            Some("review_stall_secs: 240\ncodex_review_stall_secs: 1050\n"),
            &[],
        )
        .unwrap();
        assert_eq!(policy.review_stall_secs.value, 240);
        assert_eq!(policy.review_stall_secs.source, Source::Conf);
        assert_eq!(policy.codex_review_stall_secs.value, 1050);
        assert_eq!(policy.codex_review_stall_secs.source, Source::Conf);
    }

    #[test]
    fn unknown_and_wrong_type_keys_are_hard_errors() {
        let unknown = load(Some("surprise: true\n"), &[]).unwrap_err();
        assert!(format!("{unknown:#}").contains("surprise"));
        let wrong = load(Some("daily_budget_usd: \"lots\"\n"), &[]).unwrap_err();
        assert!(format!("{wrong:#}").contains("daily_budget_usd"));
    }

    #[test]
    fn timeout_entries_report_individual_sources() {
        let policy = load(
            Some("tier_timeout_secs: {\"0\": 2400, \"1\": 3600}\n"),
            &[("FOREMAN_TIER1_TIMEOUT_SECS", "7200")],
        )
        .unwrap();
        assert_eq!(policy.tier_timeout_secs[&0].source, Source::Conf);
        assert_eq!(policy.tier_timeout_secs[&1].source, Source::Env);
        assert_eq!(policy.tier_timeout_secs[&2].source, Source::Default);
    }

    #[test]
    fn ladder_patience_accepts_per_rung_map_with_default() {
        let policy = load(
            Some("ladder_patience: {\"default\": 3, \"glm\": 1, \"claude:opus\": 4}\n"),
            &[],
        )
        .unwrap();
        assert_eq!(policy.ladder_patience.value.default, 3);
        assert_eq!(policy.ladder_patience.value.per_rung["glm"], 1);
        assert_eq!(policy.ladder_patience.value.per_rung["claude:opus"], 4);
    }

    #[test]
    fn env_start_rung_outside_the_resolved_ladder_is_a_load_error() {
        // The env parser cannot know the ladder; the check runs after resolution.
        let error = load(
            Some("ladder: [\"codex\", \"claude:fable\"]\n"),
            &[("FOREMAN_START_RUNG", "5")],
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("start_rung 5"), "{message}");
        assert!(message.contains("2-rung ladder"), "{message}");
    }

    #[test]
    fn start_rung_is_honoured_and_must_exist() {
        let default_entry = load(
            Some(
                "ladder: [\"codex\", \"glm\", \"claude:sonnet\", \"claude:opus\", \"claude:fable\"]\n",
            ),
            &[],
        )
        .unwrap();
        assert_eq!(default_entry.start_rung.value, 0);
        assert_eq!(default_entry.start_rung.source, Source::Default);

        let policy = load(
            Some("ladder: [\"codex\", \"claude:fable\"]\nstart_rung: 1\n"),
            &[],
        )
        .unwrap();
        assert_eq!(policy.start_rung.value, 1);
        assert_eq!(policy.start_rung.source, Source::Conf);
        assert_eq!(
            policy.ladder().rung_for("high", 0).unwrap().to_string(),
            "claude:fable"
        );

        let error = load(
            Some("ladder: [\"codex\", \"claude:fable\"]\nstart_rung: 7\n"),
            &[],
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("start_rung 7"), "{message}");
        assert!(message.contains("2-rung ladder"), "{message}");
    }

    #[test]
    fn every_config_key_and_env_override_is_resolved() {
        let policy = load(
            Some(
                "ladder: [\"glm\"]\n\
                 start_rung: 0\n\
                 ladder_patience: 3\n\
                 branch_contract_limit: 5\n\
                 daily_budget_usd: 30\n\
                 daily_output_tokens: 3000\n\
                 review_model: \"sonnet\"\n\
                 codex_review_model: \"gpt-config\"\n\
                 review_stall_secs: 301\n\
                 codex_review_stall_secs: 901\n\
                 review_primary: \"codex\"\n\
                 review_secondary: \"claude\"\n\
                 review_override: \"claude\"\n\
                 two_arm_review: false\n\
                 reserve_usd: 3\n\
                 reserve_tokens: 300\n\
                 tier_timeout_secs: {\"0\": 1200, \"1\": 2400, \"2\": 4800}\n\
                 tier2_commands: [\"cargo test --release\"]\n\
                 scratch_terminal_age_hours: 12\n\
                 scratch_pool: \"confpool\"\n\
                 scratch_pressure_percent: 81\n\
                 scratch_shared_max_gb: 180\n",
            ),
            &[
                ("FOREMAN_LADDER", "codex"),
                ("FOREMAN_START_RUNG", "0"),
                ("FOREMAN_LADDER_PATIENCE", "4"),
                ("FOREMAN_BRANCH_CONTRACT_LIMIT", "6"),
                ("FOREMAN_DAILY_BUDGET_USD", "40"),
                ("FOREMAN_DAILY_OUTPUT_TOKENS", "4000"),
                ("FOREMAN_REVIEW_MODEL", "opus"),
                ("FOREMAN_CODEX_REVIEW_MODEL", "gpt-env"),
                ("FOREMAN_REVIEW_STALL_SECS", "302"),
                ("FOREMAN_CODEX_REVIEW_STALL_SECS", "902"),
                ("FOREMAN_REVIEW_PRIMARY", "claude"),
                ("FOREMAN_REVIEW_SECONDARY", "codex"),
                ("FOREMAN_REVIEW_OVERRIDE", "codex"),
                ("FOREMAN_TWO_ARM_REVIEW", "true"),
                ("FOREMAN_RESERVE_USD", "4"),
                ("FOREMAN_RESERVE_TOKENS", "400"),
                ("FOREMAN_TIER0_TIMEOUT_SECS", "1300"),
                ("FOREMAN_TIER1_TIMEOUT_SECS", "2500"),
                ("FOREMAN_TIER2_TIMEOUT_SECS", "4900"),
                ("FOREMAN_TIER2_COMMANDS", "cargo test;;cargo clippy"),
                ("FOREMAN_SCRATCH_TERMINAL_AGE_HOURS", "6"),
                ("FOREMAN_SCRATCH_POOL", "envpool"),
                ("FOREMAN_SCRATCH_PRESSURE_PERCENT", "82"),
                ("FOREMAN_SCRATCH_SHARED_MAX_GB", "190"),
            ],
        )
        .unwrap();
        assert_eq!(policy.ladder.value[0].to_string(), "codex");
        assert_eq!(policy.start_rung.value, 0);
        assert_eq!(policy.ladder_patience.value.default, 4);
        assert_eq!(policy.branch_contract_limit.value, 6);
        assert_eq!(policy.daily_budget_usd.value, 40.0);
        assert_eq!(policy.daily_output_tokens.value, 4000);
        assert_eq!(policy.review_model.value, "opus");
        assert_eq!(policy.codex_review_model.value, "gpt-env");
        assert_eq!(policy.review_stall_secs.value, 302);
        assert_eq!(policy.codex_review_stall_secs.value, 902);
        assert_eq!(policy.review_primary.value, AgentKind::Claude);
        assert_eq!(policy.review_secondary.value, AgentKind::Codex);
        assert_eq!(policy.review_override.value, Some(AgentKind::Codex));
        assert!(policy.two_arm_review.value);
        assert_eq!(policy.reserve_usd.value, 4.0);
        assert_eq!(policy.reserve_tokens.value, 400);
        assert_eq!(policy.tier_timeout_secs[&0].value, 1300);
        assert_eq!(policy.tier_timeout_secs[&1].value, 2500);
        assert_eq!(policy.tier_timeout_secs[&2].value, 4900);
        assert_eq!(policy.tier2_commands.value, ["cargo test", "cargo clippy"]);
        assert_eq!(policy.scratch_terminal_age_hours.value, 6);
        assert_eq!(policy.scratch_pool.value.as_deref(), Some("envpool"));
        assert_eq!(policy.scratch_pressure_percent.value, 82);
        assert_eq!(policy.scratch_shared_max_gb.value, 190);
        assert!(
            [
                policy.ladder.source,
                policy.start_rung.source,
                policy.ladder_patience.source,
                policy.branch_contract_limit.source,
                policy.daily_budget_usd.source,
                policy.daily_output_tokens.source,
                policy.review_model.source,
                policy.codex_review_model.source,
                policy.review_stall_secs.source,
                policy.codex_review_stall_secs.source,
                policy.review_primary.source,
                policy.review_secondary.source,
                policy.review_override.source,
                policy.two_arm_review.source,
                policy.reserve_usd.source,
                policy.reserve_tokens.source,
                policy.tier2_commands.source,
                policy.scratch_terminal_age_hours.source,
                policy.scratch_pool.source,
                policy.scratch_pressure_percent.source,
                policy.scratch_shared_max_gb.source,
            ]
            .into_iter()
            .all(|source| source == Source::Env)
        );
        assert!(
            policy
                .tier_timeout_secs
                .values()
                .all(|entry| entry.source == Source::Env)
        );
    }

    #[test]
    fn foreman_conf_does_not_override_the_selected_path() {
        let dir = tempfile::tempdir().unwrap();
        let default = dir.path().join("default.conf.mix");
        let selected = dir.path().join("selected.conf.mix");
        std::fs::write(&default, "daily_budget_usd: 66\n").unwrap();
        std::fs::write(&selected, "daily_budget_usd: 77\n").unwrap();
        let policy = FleetPolicy::load_with(default.clone(), |key| {
            (key == "FOREMAN_CONF").then(|| selected.clone().into_os_string())
        })
        .unwrap();
        assert_eq!(policy.path, default);
        assert_eq!(policy.daily_budget_usd.value, 66.0);
        assert_eq!(policy.daily_budget_usd.source, Source::Conf);
    }

    #[test]
    fn glm_and_unknown_review_overrides_are_hard_errors() {
        for value in ["glm", "future"] {
            let error = load(None, &[("FOREMAN_REVIEW_OVERRIDE", value)]).unwrap_err();
            assert!(
                format!("{error:#}").contains("FOREMAN_REVIEW_OVERRIDE"),
                "{error:#}"
            );
        }
        let error = load(Some("review_override: \"glm\"\n"), &[]).unwrap_err();
        assert!(
            format!("{error:#}").contains("review_override"),
            "{error:#}"
        );
    }

    #[test]
    fn primary_and_secondary_reviewers_must_be_distinct_authorised_families() {
        let same = load(
            Some("review_primary: \"codex\"\nreview_secondary: \"codex\"\n"),
            &[],
        )
        .unwrap_err();
        assert!(
            format!("{same:#}").contains("must name different"),
            "{same:#}"
        );

        let glm = load(Some("review_primary: \"glm\"\n"), &[]).unwrap_err();
        assert!(format!("{glm:#}").contains("review_primary"), "{glm:#}");
    }
}
