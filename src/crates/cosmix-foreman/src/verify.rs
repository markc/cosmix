//! Tier-0 verification: the fast gate an agent's work must pass before a
//! task may be marked done, and before the refinery lands a branch. Verdicts
//! are computed from raw command output — never by asking the agent whether
//! it passed — and every bounce carries the failure tail, not a summary.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Per-command output tail kept in the report.
const TAIL_BYTES: usize = 8 * 1024;
/// Each best-effort attribution probe gets a short share of the step's
/// existing deadline. Probe output is diagnostic data, never a verdict.
const SCCACHE_DIAGNOSTIC_TIMEOUT: Duration = Duration::from_secs(2);
const SCCACHE_TRIGGER: &str = "sccache: error: Operation not permitted";
const SCCACHE_BYPASSED_ANNOTATION: &str = "sccache bypassed after transient EPERM";
/// A tier-0 gate is fast by definition; a command that outlives this is a
/// failure, not a longer wait.
pub const TIER0_TIMEOUT: Duration = Duration::from_secs(600);

/// Which lifecycle asked for a verification verdict. The identity travels
/// with the gate request so a later transport cannot infer it from a tier or
/// silently merge completion and landing capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateIdentity {
    RunnerCompletion,
    McpCompletion,
    RefineryTier,
    OperatorVerify,
}

/// The three Git commits which make up one Cosmix source snapshot.
///
/// Paths are deliberately absent: a worker will have different paths. The
/// commits are control data; [`LocalGateRunner`] retains today's behaviour
/// and runs the already-present local checkout without fetching or checking
/// out anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSnapshot {
    pub bus: String,
    pub cos: String,
    pub mix: String,
    /// True when `cos` is the task worktree's HEAD. False is retained only
    /// for the pre-existing local-library contract which permits a verifier
    /// fixture outside Git; a future remote runner must refuse that shape.
    #[serde(default)]
    pub worktree_commit: bool,
}

impl SourceSnapshot {
    /// Capture the exact commits visible to a local Cosmix gate. Task
    /// worktrees are siblings of the fleet's `.bus` and `.mix` clones. An
    /// explicit sibling policy wins; the build checkout is a final fallback
    /// for standalone library tests and operator invocations.
    pub fn capture(worktree_root: &Path, policy: &crate::config::FleetPolicy) -> Result<Self> {
        let bus = source_sibling(worktree_root, policy, ".bus")?;
        let mix = source_sibling(worktree_root, policy, ".mix")?;
        Ok(Self {
            bus: git_head(&bus, ".bus")?,
            cos: git_head(worktree_root, ".cos")?,
            mix: git_head(&mix, ".mix")?,
            worktree_commit: true,
        })
    }

    fn capture_for_local(
        worktree_root: &Path,
        policy: &crate::config::FleetPolicy,
    ) -> Result<Self> {
        let dot_git = worktree_root.join(".git");
        let git_metadata = match std::fs::symlink_metadata(&dot_git) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(
                    anyhow::Error::new(error).context(format!("inspecting {}", dot_git.display()))
                );
            }
        };
        let names_real_worktree = git_metadata.as_ref().is_some_and(|metadata| {
            metadata.is_file() || (metadata.is_dir() && dot_git.join("HEAD").is_file())
        });
        if names_real_worktree {
            return Self::capture(worktree_root, policy);
        }
        // LocalGateRunner historically accepts non-Git directories: `none`
        // profiles and Cargo error fixtures depend on it. Do not turn those
        // runnable local verdicts into infrastructure failures during a
        // behaviour-preserving seam extraction. A linked-worktree `.git`
        // file or a real repository directory never falls through here:
        // corruption in either remains an infrastructure error. The empty
        // `.git` directory used by historical lane-only fixtures is not a
        // repository and retains its old local-only behaviour.
        // The flag makes the legacy case explicit and non-transportable.
        let mut snapshot = Self::capture(build_source_root()?, policy)?;
        snapshot.worktree_commit = false;
        Ok(snapshot)
    }
}

/// Immutable policy owned by a gate request. Cloning the invocation's
/// already-resolved [`FleetPolicy`](crate::config::FleetPolicy) is
/// intentional: command features, timeouts and lane selection cannot change
/// underneath a long-running gate even if another thread reloads policy.
#[derive(Debug, Clone)]
pub struct GatePolicySnapshot(crate::config::FleetPolicy);

impl GatePolicySnapshot {
    pub fn new(policy: &crate::config::FleetPolicy) -> Self {
        Self(policy.clone())
    }

    fn policy(&self) -> &crate::config::FleetPolicy {
        &self.0
    }
}

/// Everything required to run the whole logical verification gate.
///
/// In particular this is above profile cwd resolution, Cargo metadata,
/// verifier/clone lanes, target isolation and executable provenance. A
/// transport added later must move this request as one unit rather than
/// execute any of those host-dependent pieces on the controller.
#[derive(Debug)]
pub struct GateRequest<'a> {
    pub task_id: i64,
    pub claim_generation: i64,
    pub identity: GateIdentity,
    pub tier: u8,
    pub source: SourceSnapshot,
    pub worktree_root: &'a Path,
    pub profile: &'a Profile,
    pub subdir: Option<&'a str>,
    pub crates: &'a [String],
    pub policy: GatePolicySnapshot,
}

impl<'a> GateRequest<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn local(
        task_id: i64,
        claim_generation: i64,
        identity: GateIdentity,
        tier: u8,
        worktree_root: &'a Path,
        profile: &'a Profile,
        subdir: Option<&'a str>,
        crates: &'a [String],
        policy: &crate::config::FleetPolicy,
    ) -> Result<Self> {
        let source = SourceSnapshot::capture_for_local(worktree_root, policy)?;
        Ok(Self {
            task_id,
            claim_generation,
            identity,
            tier,
            source,
            worktree_root,
            profile,
            subdir,
            crates,
            policy: GatePolicySnapshot::new(policy),
        })
    }

    /// Operator `foreman verify` constructor. Unlike the three task-bearing
    /// gates, this command may have no task or claim generation. It still
    /// uses the same explicit local-only unversioned-fixture marker described
    /// by [`SourceSnapshot::worktree_commit`].
    #[allow(clippy::too_many_arguments)]
    pub fn operator_local(
        tier: u8,
        worktree_root: &'a Path,
        profile: &'a Profile,
        subdir: Option<&'a str>,
        crates: &'a [String],
        policy: &crate::config::FleetPolicy,
        task: Option<(i64, i64)>,
    ) -> Result<Self> {
        let (task_id, claim_generation) = task.unwrap_or((0, 0));
        let source = SourceSnapshot::capture_for_local(worktree_root, policy)?;
        Ok(Self {
            task_id,
            claim_generation,
            identity: GateIdentity::OperatorVerify,
            tier,
            source,
            worktree_root,
            profile,
            subdir,
            crates,
            policy: GatePolicySnapshot::new(policy),
        })
    }
}

/// Verification lifecycle seam. Runnable red gates are successful calls
/// whose report has `pass == false`; `Err` means no verdict was established.
pub trait GateRunner: Send + Sync {
    fn run_gate(&self, req: &GateRequest<'_>) -> Result<VerifyReport>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct LocalGateRunner;

pub const LOCAL_GATE_RUNNER: LocalGateRunner = LocalGateRunner;

impl GateRunner for LocalGateRunner {
    fn run_gate(&self, req: &GateRequest<'_>) -> Result<VerifyReport> {
        let dir = req
            .profile
            .resolve_cwd(req.worktree_root, req.subdir)
            .map_err(|error| {
                if req.identity == GateIdentity::RefineryTier {
                    anyhow::Error::new(GateDirectoryFailure(error))
                } else {
                    error
                }
            })?;
        run_tier_for_crates_with_policy(
            req.profile,
            req.tier,
            &dir,
            req.policy.policy(),
            req.crates,
        )
    }
}

/// Typed only so the refinery can preserve its established distinction
/// between a branch-deleted verifier directory (task red) and host I/O
/// failure (infrastructure) after cwd resolution moved behind GateRunner.
#[derive(Debug)]
pub(crate) struct GateDirectoryFailure(pub anyhow::Error);

impl std::fmt::Display for GateDirectoryFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

impl std::error::Error for GateDirectoryFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}

fn git_head(repo: &Path, label: &str) -> Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--verify", "HEAD"])
        .current_dir(repo)
        .stdin(std::process::Stdio::null())
        .output()
        .with_context(|| format!("spawning git to snapshot {label} at {}", repo.display()))?;
    anyhow::ensure!(
        output.status.success(),
        "snapshotting {label} at {} failed: {}",
        repo.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let commit = String::from_utf8(output.stdout)
        .with_context(|| format!("{label} HEAD is not utf-8"))?
        .trim()
        .to_string();
    anyhow::ensure!(!commit.is_empty(), "{label} HEAD was empty");
    Ok(commit)
}

fn source_sibling(
    worktree_root: &Path,
    policy: &crate::config::FleetPolicy,
    name: &str,
) -> Result<PathBuf> {
    let configured = policy
        .sibling_repos
        .value
        .as_deref()
        .map(std::ffi::OsStr::new)
        .map(std::env::split_paths)
        .into_iter()
        .flatten()
        .find(|path| path.file_name().is_some_and(|file| file == name));
    let beside_worktree = worktree_root.parent().map(|parent| parent.join(name));
    let build_root = build_source_root()
        .ok()
        .and_then(Path::parent)
        .map(|parent| parent.join(name));
    configured
        .into_iter()
        .chain(beside_worktree)
        .chain(build_root)
        .find(|path| path.is_dir())
        .with_context(|| {
            format!(
                "cannot locate source sibling {name} for gate rooted at {}",
                worktree_root.display()
            )
        })
}

fn build_source_root() -> Result<&'static Path> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .context("Foreman build source has no repository root")
}

#[cfg(test)]
mod gate_runner_tests {
    use super::*;

    fn policy(root: &Path) -> crate::config::FleetPolicy {
        let mut policy = crate::config::FleetPolicy::defaults();
        policy.verify_lane = crate::config::Sourced {
            value: root.join("verify.lock"),
            source: crate::config::Source::Project,
        };
        policy
    }

    fn source() -> SourceSnapshot {
        SourceSnapshot {
            bus: "bus-commit".into(),
            cos: "cos-commit".into(),
            mix: "mix-commit".into(),
            worktree_commit: true,
        }
    }

    fn manifest_profile(name: &str, cwd: Option<&str>, command: Vec<String>) -> Profile {
        let step = ProfileStep {
            argv: command,
            opaque: false,
        };
        Profile::manifest(
            name.to_string(),
            cwd.map(str::to_string),
            [vec![step.clone()], vec![step], Vec::new()],
        )
    }

    fn git(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn committed_repo(path: &Path, marker: &str) -> String {
        std::fs::create_dir_all(path).unwrap();
        git(path, &["init", "--quiet"]);
        git(path, &["config", "user.name", "Gate Test"]);
        git(path, &["config", "user.email", "gate@example.com"]);
        std::fs::write(path.join("marker"), marker).unwrap();
        git(path, &["add", "marker"]);
        git(path, &["commit", "--quiet", "-m", marker]);
        git(path, &["rev-parse", "HEAD"])
    }

    #[test]
    fn local_request_carries_claim_sources_scope_and_an_owned_policy_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let bus = committed_repo(&temp.path().join(".bus"), "bus");
        let mix = committed_repo(&temp.path().join(".mix"), "mix");
        let root = temp.path().join("task-17");
        let cos = committed_repo(&root, "cos");
        let profile = manifest_profile("fixture", Some("."), vec!["true".into()]);
        let crates = vec!["cosmix-foreman".to_string()];
        let mut policy = policy(temp.path());
        policy.feature_sets.value = Some("cosmix-foreman:fixture".into());

        let request = GateRequest::local(
            17,
            4,
            GateIdentity::RunnerCompletion,
            0,
            &root,
            &profile,
            Some("src"),
            &crates,
            &policy,
        )
        .unwrap();
        policy.feature_sets.value = Some("changed-after-request".into());

        assert_eq!(request.task_id, 17);
        assert_eq!(request.claim_generation, 4);
        assert_eq!(request.identity, GateIdentity::RunnerCompletion);
        assert_eq!(request.tier, 0);
        assert_eq!(
            request.source,
            SourceSnapshot {
                bus,
                cos,
                mix,
                worktree_commit: true,
            }
        );
        assert_eq!(request.worktree_root, root);
        assert_eq!(request.profile, &profile);
        assert_eq!(request.subdir, Some("src"));
        assert_eq!(request.crates, crates);
        assert_eq!(
            request.policy.policy().feature_sets.value.as_deref(),
            Some("cosmix-foreman:fixture")
        );
    }

    #[test]
    fn present_but_broken_worktree_git_is_infrastructure_not_a_fallback_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("task-18");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join(".git"), "not a gitdir pointer\n").unwrap();
        let profile = manifest_profile("fixture", None, vec!["true".into()]);
        let error = GateRequest::local(
            18,
            1,
            GateIdentity::McpCompletion,
            0,
            &root,
            &profile,
            None,
            &[],
            &policy(temp.path()),
        )
        .expect_err("a corrupt task worktree must not borrow the build checkout's commit");
        assert!(error.to_string().contains("snapshotting .cos"), "{error:#}");
    }

    #[test]
    fn local_runner_preserves_runnable_red_and_infrastructure_error() {
        let temp = tempfile::tempdir().unwrap();
        let policy = policy(temp.path());
        let red_profile = manifest_profile("red", None, vec!["false".into()]);
        let red = GateRequest {
            task_id: 1,
            claim_generation: 2,
            identity: GateIdentity::RunnerCompletion,
            tier: 0,
            source: source(),
            worktree_root: temp.path(),
            profile: &red_profile,
            subdir: None,
            crates: &[],
            policy: GatePolicySnapshot::new(&policy),
        };
        let report = LOCAL_GATE_RUNNER.run_gate(&red).unwrap();
        assert!(!report.pass, "a runnable red gate must remain Ok(red)");

        let missing_profile = manifest_profile("missing", Some("absent"), vec!["true".into()]);
        let missing = GateRequest {
            profile: &missing_profile,
            policy: GatePolicySnapshot::new(&policy),
            ..red
        };
        assert!(
            LOCAL_GATE_RUNNER.run_gate(&missing).is_err(),
            "a gate without an established verdict must remain Err"
        );
    }

    #[test]
    fn local_runner_matches_the_old_whole_report_including_provenance_and_uncovered_ground() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("src")).unwrap();
        std::fs::write(
            temp.path().join("Cargo.toml"),
            "[package]\nname = \"gate-golden\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        std::fs::write(temp.path().join("src/lib.rs"), "#[test]\nfn golden() {}\n").unwrap();
        let profile = manifest_profile(
            "compositor",
            None,
            vec![
                "env".into(),
                "RUSTC_WRAPPER=".into(),
                "cargo".into(),
                "test".into(),
                "--quiet".into(),
            ],
        );
        let policy = policy(temp.path());
        let dir = profile.resolve_cwd(temp.path(), None).unwrap();

        // This is the pre-seam direct function. It is private, so production
        // callers cannot compile a bypass, but the golden retains a precise
        // behavioural comparison while the extraction lands.
        let old = run_tier_for_crates_with_policy(&profile, 1, &dir, &policy, &[]).unwrap();
        let request = GateRequest {
            task_id: 9,
            claim_generation: 3,
            identity: GateIdentity::RefineryTier,
            tier: 1,
            source: source(),
            worktree_root: temp.path(),
            profile: &profile,
            subdir: None,
            crates: &[],
            policy: GatePolicySnapshot::new(&policy),
        };
        let local = LOCAL_GATE_RUNNER.run_gate(&request).unwrap();

        assert_eq!(
            serde_json::to_value(&local).unwrap(),
            serde_json::to_value(&old).unwrap(),
            "every persisted report field must survive the GateRunner seam"
        );
        assert!(
            !local.uncovered.is_empty(),
            "golden must cover uncovered ground"
        );
        assert_eq!(local.provenance_tier, Some(1));
        assert!(
            local
                .steps
                .iter()
                .any(|step| step.executed_binaries.is_some()),
            "golden must cover executable provenance"
        );
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyReport {
    pub profile: String,
    /// Whether this report came from an unattended gate or an explicitly
    /// requested physical acceptance run. Old persisted reports predate the
    /// distinction and deserialize as headless: every historical verifier
    /// entry was produced by the unattended path.
    #[serde(default)]
    pub execution: VerifyExecution,
    pub pass: bool,
    pub steps: Vec<VerifyStep>,
    /// Ground this run deliberately did not cover. A green headless report
    /// is honest only when these limits travel with the verdict itself.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uncovered: Vec<UncoveredGround>,
    /// The PRIVATE, per-worktree target directory [`crate::target_dir`]
    /// verified cargo would use for this run's commands, when the working
    /// directory is a Cargo workspace — legibility for whoever next has to
    /// work out which directory a report's binaries came from. This is the RESOLVED
    /// directory cargo actually reported via `cargo metadata`, not merely
    /// the ambient environment variable — see the `target_dir` module for
    /// why that distinction is load-bearing. `#[serde(default)]` so a report
    /// persisted before this field existed still deserializes.
    #[serde(default)]
    pub target_dir: Option<String>,
    /// Tier which paid the one verifier-owned executable-provenance probe.
    /// Tier 0 deliberately leaves this absent: its only build provenance is
    /// the resolved private `target_dir`. Landing tier 1 carries at most one
    /// probe, on the principal test step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance_tier: Option<u8>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyExecution {
    #[default]
    Headless,
    PhysicalAcceptance,
}

impl VerifyExecution {
    pub fn label(self) -> &'static str {
        match self {
            Self::Headless => "HEADLESS",
            Self::PhysicalAcceptance => "PHYSICAL ACCEPTANCE",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UncoveredGround {
    pub area: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyStep {
    pub command: String,
    pub pass: bool,
    pub exit_code: Option<i32>,
    pub tail: String,
    /// Non-verdict notes about how this logical step completed. Empty on
    /// reports written before annotations existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub annotations: Vec<String>,
    /// One-shot evidence captured after an observed sccache EPERM. This is
    /// deliberately structured: attribution must not be lost inside a
    /// truncated command tail, and probe failures remain visible as data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sccache_incident: Option<SccacheIncident>,
    /// Diagnostic-only hashes for the ONE tier-1 test step selected to carry
    /// landing provenance. Absent on tier 0 and every other step. When
    /// present, uncertainty is explicit in [`BinaryProvenance`] and cannot
    /// change `pass`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executed_binaries: Option<crate::provenance::BinaryProvenance>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticProbe {
    pub command: String,
    pub exit_code: Option<i32>,
    pub output: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NofileLimit {
    /// The raw `/proc/<step-pid>/limits` row preserves both soft and hard
    /// values without guessing how a particular kernel rendered infinity.
    pub raw: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrnoContext {
    pub number: i32,
    pub symbol: Option<String>,
    pub description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SccacheBypassRetry {
    NotAttempted,
    Passed,
    Failed,
}

/// Evidence for one observed sccache client-side EPERM, including the
/// wrapper-bypass outcome when the tier was eligible to retry.
///
/// Investigation hypotheses, not conclusions: pressure on inherited
/// RLIMIT_NPROC/RLIMIT_NOFILE inside concurrent memguard scopes; a
/// Landlock/seccomp inheritance edge from a Codex CLI sandbox leaking into
/// a process group; or sccache client-side jobserver-fd exhaustion. No code
/// path selects among these explanations. The verifier records the live
/// facts first and changes behaviour only on the literal observed error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SccacheIncident {
    pub original_exit_code: Option<i32>,
    pub original_stdout_tail: String,
    pub original_stderr_tail: String,
    pub server_port: u16,
    pub server_port_context: String,
    pub listener_probe: DiagnosticProbe,
    pub server_pid: Option<u32>,
    pub server_cgroup: Option<String>,
    pub server_cgroup_error: Option<String>,
    pub show_stats_probe: DiagnosticProbe,
    pub step_nofile: NofileLimit,
    pub errno: Option<ErrnoContext>,
    pub bypass_retry: SccacheBypassRetry,
}

impl VerifyReport {
    /// Human-readable failure digest for bounces and refusals.
    pub fn failure_digest(&self) -> String {
        let mut out = String::new();
        for step in self.steps.iter().filter(|s| !s.pass) {
            out.push_str(&format!(
                "$ {} (exit {:?})\n{}\n",
                step.command,
                step.exit_code,
                step.tail.trim()
            ));
            if let Some(incident) = &step.sccache_incident {
                out.push_str(&incident.render());
            }
        }
        out
    }

    /// One human-readable item per step whose sccache-free retry passed.
    /// Callers with a task/ledger association file each item separately so
    /// two incidents in one otherwise-green tier remain two countable
    /// occurrences.
    pub fn sccache_bypass_digests(&self) -> Vec<String> {
        self.steps
            .iter()
            .filter_map(|step| step.sccache_bypass_digest(&self.profile))
            .collect()
    }
}

impl VerifyStep {
    pub fn sccache_bypass_digest(&self, profile: &str) -> Option<String> {
        let incident = self.sccache_incident.as_ref()?;
        if !self.pass || incident.bypass_retry != SccacheBypassRetry::Passed {
            return None;
        }
        Some(format!(
            "profile: {}\ncommand: {}\nannotation: {}\n{}",
            profile,
            self.command,
            SCCACHE_BYPASSED_ANNOTATION,
            incident.render()
        ))
    }
}

impl SccacheIncident {
    pub fn render(&self) -> String {
        let errno = self
            .errno
            .as_ref()
            .map(|errno| {
                format!(
                    "{}{}: {}",
                    errno
                        .symbol
                        .as_deref()
                        .map(|symbol| format!("{symbol}/"))
                        .unwrap_or_default(),
                    errno.number,
                    errno.description
                )
            })
            .unwrap_or_else(|| "not extractable".to_string());
        let nofile = self
            .step_nofile
            .raw
            .as_deref()
            .map(str::to_string)
            .unwrap_or_else(|| {
                format!(
                    "unavailable: {}",
                    self.step_nofile.error.as_deref().unwrap_or("unknown error")
                )
            });
        let cgroup = self
            .server_cgroup
            .as_deref()
            .map(str::to_string)
            .unwrap_or_else(|| {
                format!(
                    "unavailable: {}",
                    self.server_cgroup_error
                        .as_deref()
                        .unwrap_or("server pid unavailable")
                )
            });
        format!(
            "--- sccache EPERM attribution ---\n\
             original exit: {:?}\n\
             original stdout (tail):\n{}\n\
             original stderr (tail):\n{}\n\
             errno: {}\n\
             wrapper-free retry: {:?}\n\
             step RLIMIT_NOFILE: {}\n\
             server port: {} ({})\n\
             listener probe: $ {} (exit {:?})\n{}\n\
             server pid: {:?}\n\
             server cgroup: {}\n\
             stats probe: $ {} (exit {:?})\n{}\n",
            self.original_exit_code,
            self.original_stdout_tail.trim(),
            self.original_stderr_tail.trim(),
            errno,
            self.bypass_retry,
            nofile,
            self.server_port,
            self.server_port_context,
            self.listener_probe.command,
            self.listener_probe.exit_code,
            self.listener_probe.output.trim(),
            self.server_pid,
            cgroup.trim(),
            self.show_stats_probe.command,
            self.show_stats_probe.exit_code,
            self.show_stats_probe.output.trim(),
        )
    }
}

/// File one informational finding for every successful sccache bypass in a
/// task-associated report. Verification is intentionally ledger-agnostic
/// until a caller supplies the task identity; direct ad-hoc verifies still
/// retain the same evidence in their report but cannot invent an owner.
pub fn file_sccache_bypass_findings(
    ledger: &crate::ledger::Ledger,
    task_id: i64,
    report: &VerifyReport,
    filed_by: &str,
) -> Result<Vec<i64>> {
    ledger.file_sccache_bypass_findings(task_id, &report.sccache_bypass_digests(), filed_by)
}

/// A verifier profile: its name, the working directory it owns, and — via
/// [`Profile::tier_commands`] — the ordered argv command lines it runs at
/// each tier.
///
/// `cwd` is relative to the repo root; `None` means "fall back to the
/// invocation's `--subdir`" — today's behaviour, unconditionally, for both
/// built-in profiles without an owned directory (`rust`, `none`). A profile
/// that names its own `cwd` verifies there regardless of how it was invoked.
///
/// The commands are a METHOD rather than a stored `Vec`: they are computed
/// lazily per requested tier so that resolving WHERE a profile runs never
/// evaluates tier 1's `cargo-deny` PATH probe or tier 2's
/// tier-2 policy expansion. These used to be eager fields, which ran
/// the probe and parsed the env var on every tier-0 resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    pub cwd: Option<String>,
    source: ProfileSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProfileSource {
    Builtin,
    Manifest([Vec<ProfileStep>; 3]),
}

/// One operator-authored verifier command. `opaque` is an explicit trust
/// decision: transparent argv is preflighted and pinned normally, while an
/// opaque wrapper runs with target/provenance reported as unknown. Nothing
/// infers opacity from command text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileStep {
    pub argv: Vec<String>,
    pub opaque: bool,
}

/// Opaque commands are refused because their effective cargo argv cannot be
/// probed. Any exception must be an explicit built-in profile declaration by
/// `(profile, tier, step index)` here; it is never inferred from shell text or
/// supplied by the task. There are intentionally no exceptions today.
const OPAQUE_PROFILE_STEPS: &[(&str, u8, usize)] = &[];

fn profile_step_is_opaque(profile: &Profile, tier: u8, step: usize) -> bool {
    OPAQUE_PROFILE_STEPS.contains(&(profile.name.as_str(), tier, step))
}

impl Profile {
    /// Construct a manifest-owned profile. The manifest parser has already
    /// validated names, cwd and non-empty argv; this constructor keeps the
    /// verifier's executable profile shape in one module.
    pub fn manifest(name: String, cwd: Option<String>, tiers: [Vec<ProfileStep>; 3]) -> Self {
        Self {
            name,
            cwd,
            source: ProfileSource::Manifest(tiers),
        }
    }

    /// Resolve this profile's working directory: use `cwd` if set, otherwise
    /// fall back to the invocation's `--subdir`. The returned path is
    /// canonicalized and CONTAINED in `workdir` (`resolve_verify_dir`'s
    /// containment, added in 0.6.2 after a committed symlink could have
    /// laundered verification against unrelated green code) — a profile is
    /// not a way around that.
    pub fn resolve_cwd(&self, workdir: &Path, subdir: Option<&str>) -> Result<PathBuf> {
        let target = self.cwd.as_deref().or(subdir);
        crate::runner::resolve_verify_dir(workdir, target)
    }

    /// This profile's ordered argv command lines for `tier` — see
    /// [`tier_commands`] for the tier structure and why this is computed on
    /// demand rather than stored.
    pub fn tier_commands(&self, tier: u8) -> Result<Vec<Vec<String>>> {
        let policy = default_policy()?;
        tier_commands_of(self, tier, None, &policy, &[])
    }

    pub fn tier_commands_with_policy(
        &self,
        tier: u8,
        policy: &crate::config::FleetPolicy,
    ) -> Result<Vec<Vec<String>>> {
        tier_commands_of(self, tier, None, policy, &[])
    }

    /// Resolve commands that depend on the profile's actual working tree.
    /// Rust tier 1 uses this form because Cargo feature discovery is a
    /// property of the manifests being verified, not of Foreman's own cwd.
    pub fn tier_commands_in_dir_with_policy(
        &self,
        tier: u8,
        dir: &Path,
        policy: &crate::config::FleetPolicy,
    ) -> Result<Vec<Vec<String>>> {
        tier_commands_of(self, tier, Some(dir), policy, &[])
    }

    /// Directory-aware command construction for a task's operator-owned
    /// crate scope. An empty slice is the backwards-compatible whole
    /// workspace behaviour.
    pub fn tier_commands_for_crates_in_dir_with_policy(
        &self,
        tier: u8,
        dir: &Path,
        policy: &crate::config::FleetPolicy,
        crates: &[String],
    ) -> Result<Vec<Vec<String>>> {
        tier_commands_of(self, tier, Some(dir), policy, crates)
    }

    pub fn workspace_subdir(&self, fallback: Option<&str>) -> Option<String> {
        self.cwd.clone().or_else(|| fallback.map(str::to_string))
    }

    fn opaque_steps(&self, tier: u8, command_count: usize) -> Result<Vec<bool>> {
        anyhow::ensure!(tier <= 2, "unknown verifier tier {tier} (known: 0, 1, 2)");
        match &self.source {
            ProfileSource::Builtin => Ok((0..command_count)
                .map(|step| profile_step_is_opaque(self, tier, step))
                .collect()),
            ProfileSource::Manifest(tiers) => Ok(tiers[tier as usize]
                .iter()
                .map(|step| step.opaque)
                .collect()),
        }
    }
}

/// Look up a profile's identity (name + owned cwd, if any).
///
/// WHERE PROFILES ARE DEFINED — decided here, deliberately: **built-in, in
/// code, in this table**. Not a config file. Fleet task 27 is introducing
/// `foreman.conf.mix`, and defining profiles there in the same window would
/// collide with it head-on; this table can be layered under a config
/// override later without changing the `Profile` shape or any call site.
/// Tasks name a profile in their `verifier_profile` column; unknown names
/// are an error, not a silent skip.
///
/// Kept separate from [`tier_commands`] so
/// resolving WHERE a profile runs never has to build every tier's command
/// list — tier 1's `cargo-deny` PATH probe and tier 2's
/// tier-2 command expansion must not fire just to find tier 0's
/// directory (they used to, when a profile eagerly built all three tiers up
/// front — that ran the probe and parsed the env var on EVERY tier-0
/// resolution, not just tier-1/2 runs). A new profile is one more row in
/// [`BUILTIN_PROFILES`].
#[derive(Debug)]
struct BuiltinProfile {
    name: &'static str,
    cwd: Option<&'static str>,
}

/// The one source of truth for built-in verifier identities and their owned
/// directories. Lookup, public name enumeration, CLI help and unknown-name
/// errors are all derived from this table.
const BUILTIN_PROFILES: &[BuiltinProfile] = &[
    BuiltinProfile {
        name: "rust",
        cwd: None,
    },
    BuiltinProfile {
        name: "compositor",
        // Owns its cwd: desktop/ is a SEPARATE cargo workspace (its own
        // resolver, its own [patch.crates-io] for the vendored smithay/wgpu
        // forks) — not a subdirectory of the `src/` tree the rust profile
        // verifies. `--subdir` is never consulted for this profile.
        cwd: Some("desktop"),
    },
    BuiltinProfile {
        name: "none",
        cwd: None,
    },
];

static BUILTIN_PROFILE_NAMES: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    BUILTIN_PROFILES
        .iter()
        .map(|profile| profile.name)
        .collect()
});

/// Every legal built-in profile name, in the same order as the table used by
/// [`builtin_profile`]. This is the authoring/help surface; the empty string is
/// a backwards-compatible alias for `rust`, not a profile identity.
pub fn builtin_profile_names() -> &'static [&'static str] {
    BUILTIN_PROFILE_NAMES.as_slice()
}

fn builtin_profile(name: &str) -> Result<Profile> {
    let canonical_name = if name.is_empty() { "rust" } else { name };
    let profile = BUILTIN_PROFILES
        .iter()
        .find(|profile| profile.name == canonical_name)
        .with_context(|| {
            format!(
                "unknown verifier profile {name:?} (known: {})",
                builtin_profile_names().join(", ")
            )
        })?;
    Ok(Profile {
        name: profile.name.to_string(),
        cwd: profile.cwd.map(str::to_string),
        source: ProfileSource::Builtin,
    })
}

/// Public entry point for [`builtin_profile`] — callers that need to
/// distinguish "unknown profile name" from "this profile's cwd could not be
/// resolved" (the refinery bounces the latter as a task problem but
/// propagates the former as before, unchanged) look the profile up here
/// first, then call [`Profile::resolve_cwd`] themselves.
pub fn lookup_profile(name: &str) -> Result<Profile> {
    builtin_profile(name)
}

/// Resolve a profile's working directory without running anything — shared
/// by tier 0 ([`run_profile`]) and every tier-1/2 caller (the refinery's
/// pre-land gate, `foreman verify`) so a profile that owns a `cwd` is
/// honoured at every tier it runs at, not just at completion.
pub fn resolve_profile_dir(profile: &str, workdir: &Path, subdir: Option<&str>) -> Result<PathBuf> {
    builtin_profile(profile)?.resolve_cwd(workdir, subdir)
}

/// The workspace-relative directory whose target this profile verifies.
/// An explicit profile cwd outranks the fleet's `--subdir` fallback, matching
/// [`Profile::resolve_cwd`] without requiring the directory to exist before
/// the agent starts.
#[cfg(test)]
pub(crate) fn profile_workspace_subdir(
    name: &str,
    fallback: Option<&str>,
) -> Result<Option<String>> {
    let profile = builtin_profile(name)?;
    Ok(profile.workspace_subdir(fallback))
}

/// A named profile's tier-0 commands — see [`builtin_profile`] for where
/// profiles are defined.
pub fn profile_commands(profile: &str) -> Result<Vec<Vec<String>>> {
    tier_commands(profile, 0)
}

/// The tier structure (plan §5):
/// - tier 0 (seconds): fmt / clippy / crate tests — the completion gate.
/// - tier 1 (minutes): tier 0 plus full workspace tests, one test invocation
///   per auto-discovered non-default Cargo feature, and `cargo deny` (when
///   installed) — the refinery's pre-land gate.
/// - tier 2 (HEADLESS, nightly): operator-defined by fleet policy
///   (commands separated by `;;`, argv whitespace-split — fuzz targets and
///   property suites are repo-specific by nature). Empty = an explicitly
///   empty tier, which passes with an empty report. Fleet GC is now one
///   explicit invocation per live worktree, for example `foreman gc-cache
///   --dir ~/.cmctl/.foreman/task-<id>/src/target`; it must not use the
///   removed shared-cache environment default or a literal `$VAR` (argv is
///   not shell-expanded). See the `gc` module docs and runbook for the path
///   validation and enumeration contract. Every command inherits
///   [`HEADLESS_ENV`], so it cannot enter foreman's separately named physical
///   acceptance path even if that subcommand is placed here by mistake.
///
/// Rust feature scope is deliberately per crate and per feature, never a
/// workspace-wide `--all-features`: the latter can activate mesh/citizen
/// integrations needing a live broker and make an unrelated crate fail.
/// Auto-discovery reads the resolved workspace's metadata. `_...` private
/// harnesses and the repo-wide `cosmix` live-citizen convention are reported
/// as not auto-tested; an operator can cover either explicitly with
/// `FOREMAN_FEATURE_SETS="crate:feature;;crate:feature,other"`.
/// `FOREMAN_FEATURE_EXCLUDE="crate:feature;;..."` records environment-bound
/// exclusions. An explicitly empty/malformed set, a malformed exclusion, or
/// metadata that cannot be resolved becomes a failing report step. A crate
/// with genuinely no runnable optional features gets a visible passing info
/// step, so an absent feature dimension is never mistaken for tested code.
pub fn tier_commands(profile: &str, tier: u8) -> Result<Vec<Vec<String>>> {
    builtin_profile(profile)?.tier_commands(tier)
}

pub fn tier_commands_with_policy(
    profile: &str,
    tier: u8,
    policy: &crate::config::FleetPolicy,
) -> Result<Vec<Vec<String>>> {
    builtin_profile(profile)?.tier_commands_with_policy(tier, policy)
}

/// A named profile's commands resolved against the directory it will
/// actually verify. Callers that execute the result must use this form;
/// [`tier_commands`] deliberately reports an unknown feature dimension for
/// rust tier 1 because it was not given a manifest location.
pub fn tier_commands_in_dir_with_policy(
    profile: &str,
    tier: u8,
    dir: &Path,
    policy: &crate::config::FleetPolicy,
) -> Result<Vec<Vec<String>>> {
    builtin_profile(profile)?.tier_commands_in_dir_with_policy(tier, dir, policy)
}

/// Directory-aware tier commands scoped to the named packages and every
/// workspace package which transitively depends on them. `crates=[]` is
/// intentionally identical to [`tier_commands_in_dir_with_policy`].
pub fn tier_commands_for_crates_in_dir_with_policy(
    profile: &str,
    tier: u8,
    dir: &Path,
    policy: &crate::config::FleetPolicy,
    crates: &[String],
) -> Result<Vec<Vec<String>>> {
    builtin_profile(profile)?.tier_commands_for_crates_in_dir_with_policy(tier, dir, policy, crates)
}

/// [`tier_commands`] for an already-looked-up profile — the form
/// [`run_tier`] uses, so running a tier never re-does the name lookup that
/// found the profile's directory in the first place.
fn tier_commands_of(
    p: &Profile,
    tier: u8,
    dir: Option<&Path>,
    policy: &crate::config::FleetPolicy,
    crates: &[String],
) -> Result<Vec<Vec<String>>> {
    // Tier range validated for EVERY profile — tier 3 is the review
    // verdict's slot in the verifications table, and `--profile none
    // --tier 3` must not be able to forge an empty passing row there.
    anyhow::ensure!(tier <= 2, "unknown verifier tier {tier} (known: 0, 1, 2)");
    if let ProfileSource::Manifest(tiers) = &p.source {
        return Ok(tiers[tier as usize]
            .iter()
            .map(|step| step.argv.clone())
            .collect());
    }
    if p.name == "none" {
        return Ok(Vec::new());
    }
    if p.name == "compositor" {
        return compositor_tier_commands(tier, policy);
    }
    if tier == 2 {
        return Ok(policy.tier2_argv());
    }
    // Only the "rust" identity remains — computed LAZILY per requested tier,
    // never all three up front: tier 1's cargo-deny PATH probe and tier 2's
    // Tier-2 command expansion must not fire for a tier-0 resolution.
    let package_scope = match (dir, crates.is_empty()) {
        (_, true) => Ok(None),
        (Some(dir), false) => resolve_workspace_scope(dir, crates).map(Some),
        (None, false) => Err(anyhow::anyhow!(
            "no verifier directory was supplied while resolving task crate scope"
        )),
    };
    let tier0 = match package_scope.as_ref() {
        Ok(scope) => scoped_tier0(scope.as_deref()),
        Err(error) => vec![feature_gap_command(
            "crate-scope-undiscoverable",
            "tasks.crates / cargo metadata",
            error.to_string(),
        )],
    };
    match tier {
        0 => Ok(tier0),
        1 => {
            // REPLACE the crate-level test step with the workspace suite —
            // appending would run the whole suite twice (under a virtual
            // manifest the two are identical) while holding both the
            // refinery lane and the host verify lane.
            let mut cmds = tier0;
            if package_scope.is_ok() {
                cmds.pop();
            }
            cmds.push(vec!["cargo".into(), "test".into(), "--workspace".into()]);
            append_feature_coverage(
                &mut cmds,
                dir,
                policy,
                package_scope
                    .as_ref()
                    .ok()
                    .and_then(|scope| scope.as_deref()),
            );
            if binary_on_path("cargo-deny") {
                cmds.push(vec!["cargo".into(), "deny".into(), "check".into()]);
            } else {
                eprintln!(
                    "foreman: cargo-deny not installed — tier 1 runs without the \
                     advisory/license audit"
                );
            }
            Ok(cmds)
        }
        _ => unreachable!("tier range checked above"),
    }
}

fn scoped_tier0(packages: Option<&[String]>) -> Vec<Vec<String>> {
    let mut fmt = vec!["cargo".to_string(), "fmt".into(), "--check".into()];
    let mut clippy = vec!["cargo".to_string(), "clippy".into()];
    let mut test = vec!["cargo".to_string(), "test".into()];
    if let Some(packages) = packages {
        for package in packages {
            fmt.extend(["--package".into(), package.clone()]);
            clippy.extend(["--package".into(), package.clone()]);
            test.extend(["--package".into(), package.clone()]);
        }
    }
    clippy.extend([
        "--all-targets".into(),
        "--".into(),
        "-D".into(),
        "warnings".into(),
    ]);
    vec![fmt, clippy, test]
}

/// Resolve the task's package names plus the transitive workspace reverse
/// dependencies which can be broken by their change. Cargo's resolved graph
/// is authoritative, including renamed dependencies and target-specific
/// edges; hand-parsing manifests would miss both.
fn resolve_workspace_scope(dir: &Path, requested: &[String]) -> Result<Vec<String>> {
    let metadata = cargo_metadata(dir)?;
    resolve_workspace_scope_from_metadata(&metadata, requested)
}

fn resolve_workspace_scope_from_metadata(
    metadata: &serde_json::Value,
    requested: &[String],
) -> Result<Vec<String>> {
    let member_values = metadata["workspace_members"]
        .as_array()
        .context("cargo metadata output has no workspace_members array")?;
    let mut member_ids = std::collections::HashSet::new();
    for value in member_values {
        let id = value
            .as_str()
            .context("cargo metadata workspace_members contains a non-string package id")?;
        anyhow::ensure!(
            member_ids.insert(id.to_string()),
            "cargo metadata workspace_members contains duplicate package id {id:?}"
        );
    }
    let packages = metadata["packages"]
        .as_array()
        .context("cargo metadata output has no packages array")?;
    let mut package_ids = std::collections::HashSet::new();
    let mut name_to_id = std::collections::HashMap::new();
    let mut id_to_name = std::collections::HashMap::new();
    for package in packages {
        let id = package["id"]
            .as_str()
            .context("cargo metadata package has no string id")?;
        let name = package["name"]
            .as_str()
            .context("cargo metadata package has no string name")?;
        anyhow::ensure!(
            package_ids.insert(id.to_string()),
            "cargo metadata packages contains duplicate package id {id:?}"
        );
        if member_ids.contains(id) {
            anyhow::ensure!(
                name_to_id
                    .insert(name.to_string(), id.to_string())
                    .is_none(),
                "cargo metadata workspace contains duplicate package name {name:?}"
            );
            anyhow::ensure!(
                id_to_name
                    .insert(id.to_string(), name.to_string())
                    .is_none(),
                "cargo metadata packages contains duplicate workspace package id {id:?}"
            );
        }
    }
    for id in &member_ids {
        anyhow::ensure!(
            id_to_name.contains_key(id),
            "cargo metadata workspace member {id:?} has no package record"
        );
    }

    let mut selected = std::collections::HashSet::new();
    let mut pending = std::collections::VecDeque::new();
    for package in requested {
        let id = name_to_id.get(package).with_context(|| {
            format!("task crate {package:?} is not a member of the verifier workspace")
        })?;
        if selected.insert(id.clone()) {
            pending.push_back(id.clone());
        }
    }

    let nodes = metadata["resolve"]["nodes"]
        .as_array()
        .context("cargo metadata output has no resolved dependency graph")?;
    let mut reverse: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    let mut workspace_nodes = std::collections::HashSet::new();
    for node in nodes {
        let dependent = node["id"]
            .as_str()
            .context("cargo metadata resolve node has no string id")?;
        let deps = node["deps"]
            .as_array()
            .context("cargo metadata resolve node has no deps array")?;
        if !member_ids.contains(dependent) {
            continue;
        }
        anyhow::ensure!(
            workspace_nodes.insert(dependent.to_string()),
            "cargo metadata resolve contains duplicate workspace node {dependent:?}"
        );
        for dep in deps {
            let dependency = dep["pkg"]
                .as_str()
                .context("cargo metadata resolve dependency has no string pkg id")?;
            anyhow::ensure!(
                package_ids.contains(dependency),
                "cargo metadata resolve dependency names unknown package id {dependency:?}"
            );
            if member_ids.contains(dependency) {
                reverse
                    .entry(dependency.to_string())
                    .or_default()
                    .push(dependent.to_string());
            }
        }
    }
    for id in &member_ids {
        anyhow::ensure!(
            workspace_nodes.contains(id),
            "cargo metadata workspace member {id:?} has no resolved dependency node"
        );
    }
    while let Some(dependency) = pending.pop_front() {
        for dependent in reverse.get(&dependency).into_iter().flatten() {
            if selected.insert(dependent.clone()) {
                pending.push_back(dependent.clone());
            }
        }
    }

    let mut names = selected
        .into_iter()
        .map(|id| {
            id_to_name
                .get(&id)
                .cloned()
                .with_context(|| format!("workspace member {id:?} has no package name"))
        })
        .collect::<Result<Vec<_>>>()?;
    names.sort();
    Ok(names)
}

#[cfg(test)]
mod workspace_scope_metadata_tests {
    use super::*;

    fn metadata() -> serde_json::Value {
        serde_json::json!({
            "workspace_members": ["leaf 0.1.0", "consumer 0.1.0"],
            "packages": [
                {"id": "leaf 0.1.0", "name": "leaf"},
                {"id": "consumer 0.1.0", "name": "consumer"}
            ],
            "resolve": {"nodes": [
                {"id": "leaf 0.1.0", "deps": []},
                {"id": "consumer 0.1.0", "deps": [{"pkg": "leaf 0.1.0"}]}
            ]}
        })
    }

    #[test]
    fn metadata_parser_fails_closed_instead_of_truncating_reverse_dependencies() {
        let mut malformed = metadata();
        malformed["resolve"]["nodes"][1]["deps"][0] = serde_json::json!({});
        let error = resolve_workspace_scope_from_metadata(&malformed, &["leaf".into()])
            .expect_err("a malformed dependency edge must not silently drop consumer");
        assert!(error.to_string().contains("pkg id"), "{error:#}");

        let mut malformed = metadata();
        malformed["workspace_members"][1] = serde_json::json!(7);
        let error = resolve_workspace_scope_from_metadata(&malformed, &["leaf".into()])
            .expect_err("a malformed member id must not silently disappear");
        assert!(error.to_string().contains("non-string"), "{error:#}");

        let mut malformed = metadata();
        malformed["resolve"]["nodes"][1]["deps"][0]["pkg"] = serde_json::json!("unknown 0.1.0");
        let error = resolve_workspace_scope_from_metadata(&malformed, &["leaf".into()])
            .expect_err("an unknown package id must not silently drop consumer");
        assert!(error.to_string().contains("unknown package"), "{error:#}");
    }
}

/// Result of resolving the rust tier-1 feature dimension.
enum FeatureCoverage {
    Sets {
        commands: Vec<Vec<String>>,
        skipped: Vec<String>,
    },
    NothingToTest {
        skipped: Vec<String>,
    },
    Misconfigured(String),
    Undiscoverable(String),
}

/// Add feature coverage immediately after the default workspace tests and
/// before the optional supply-chain audit. Synthetic info/gap commands are
/// interpreted by [`run_step`], so the persisted report records both honest
/// exclusions and any failure to establish coverage.
fn append_feature_coverage(
    commands: &mut Vec<Vec<String>>,
    dir: Option<&Path>,
    policy: &crate::config::FleetPolicy,
    package_scope: Option<&[String]>,
) {
    match resolve_feature_coverage(dir, policy, package_scope) {
        FeatureCoverage::Sets {
            commands: feature_commands,
            skipped,
        } => {
            if !skipped.is_empty() {
                commands.push(feature_info_command(format!(
                    "not auto-tested: {}",
                    skipped.join(", ")
                )));
            }
            commands.extend(feature_commands);
        }
        FeatureCoverage::NothingToTest { skipped } => {
            let detail = if skipped.is_empty() {
                "no optional non-default features declared".to_string()
            } else {
                format!(
                    "no automatically runnable optional features; not auto-tested: {}",
                    skipped.join(", ")
                )
            };
            commands.push(feature_info_command(detail));
        }
        FeatureCoverage::Misconfigured(detail) => commands.push(feature_gap_command(
            "feature-coverage-misconfigured",
            "FOREMAN_FEATURE_SETS/FOREMAN_FEATURE_EXCLUDE",
            detail,
        )),
        FeatureCoverage::Undiscoverable(detail) => commands.push(feature_gap_command(
            "feature-coverage-undiscoverable",
            "cargo metadata",
            detail,
        )),
    }
}

fn feature_info_command(detail: String) -> Vec<String> {
    vec![
        "foreman-verify-info".into(),
        "feature-coverage".into(),
        detail,
    ]
}

fn feature_gap_command(reason: &str, subject: &str, detail: String) -> Vec<String> {
    vec![
        "foreman-verify-gap".into(),
        reason.into(),
        subject.into(),
        detail,
    ]
}

fn resolve_feature_coverage(
    dir: Option<&Path>,
    policy: &crate::config::FleetPolicy,
    package_scope: Option<&[String]>,
) -> FeatureCoverage {
    if let Some(spec) = policy.feature_sets.value.as_deref() {
        return match configured_feature_commands(spec) {
            Ok(commands) if commands.is_empty() => {
                FeatureCoverage::Misconfigured("the configured feature set is empty".to_string())
            }
            Ok(commands) => match dir
                .map(|dir| validate_configured_feature_commands(dir, &commands))
                .transpose()
            {
                Ok(_) => {
                    let commands = filter_feature_commands(commands, package_scope);
                    if commands.is_empty() {
                        FeatureCoverage::NothingToTest {
                            skipped: Vec::new(),
                        }
                    } else {
                        FeatureCoverage::Sets {
                            commands,
                            skipped: Vec::new(),
                        }
                    }
                }
                Err(error) => FeatureCoverage::Misconfigured(error.to_string()),
            },
            Err(error) => FeatureCoverage::Misconfigured(error.to_string()),
        };
    }
    let Some(dir) = dir else {
        return FeatureCoverage::Undiscoverable(
            "no verifier directory was supplied while constructing rust tier 1".to_string(),
        );
    };
    match auto_discover_feature_commands(
        dir,
        policy.feature_exclude.value.as_deref(),
        package_scope,
    ) {
        Ok((commands, skipped)) if commands.is_empty() => {
            FeatureCoverage::NothingToTest { skipped }
        }
        Ok((commands, skipped)) => FeatureCoverage::Sets { commands, skipped },
        Err(error) => FeatureCoverage::Undiscoverable(error.to_string()),
    }
}

/// Parse `FOREMAN_FEATURE_SETS` as `crate:feature[,feature]` entries joined
/// by `;;`. Foreman owns the Cargo argv rather than accepting an arbitrary
/// command tail, so every accepted entry necessarily names a package and a
/// non-empty feature list.
fn configured_feature_commands(spec: &str) -> Result<Vec<Vec<String>>> {
    let mut commands = Vec::new();
    for (index, entry) in spec.split(";;").enumerate() {
        let entry = entry.trim();
        anyhow::ensure!(
            !entry.is_empty(),
            "feature-set entry #{} is empty",
            index + 1
        );
        let (package, features) = entry.split_once(':').with_context(|| {
            format!(
                "feature-set entry #{} must be 'crate:feature[,feature]'",
                index + 1
            )
        })?;
        let package = package.trim();
        let feature_names: Vec<&str> = features
            .split(',')
            .map(str::trim)
            .filter(|feature| !feature.is_empty())
            .collect();
        anyhow::ensure!(
            !package.is_empty() && !feature_names.is_empty(),
            "feature-set entry #{} must name both a crate and at least one feature",
            index + 1
        );
        anyhow::ensure!(
            feature_names.len() == features.split(',').count(),
            "feature-set entry #{} contains an empty feature name",
            index + 1
        );
        commands.push(vec![
            "cargo".into(),
            "test".into(),
            "-p".into(),
            package.into(),
            "--features".into(),
            feature_names.join(","),
        ]);
    }
    Ok(commands)
}

fn filter_feature_commands(
    commands: Vec<Vec<String>>,
    package_scope: Option<&[String]>,
) -> Vec<Vec<String>> {
    let Some(package_scope) = package_scope else {
        return commands;
    };
    let allowed: std::collections::HashSet<&str> =
        package_scope.iter().map(String::as_str).collect();
    commands
        .into_iter()
        .filter(|command| {
            command
                .get(3)
                .is_some_and(|name| allowed.contains(name.as_str()))
        })
        .collect()
}

fn validate_configured_feature_commands(dir: &Path, commands: &[Vec<String>]) -> Result<()> {
    let metadata = cargo_metadata(dir)?;
    let workspace_members: std::collections::HashSet<&str> = metadata["workspace_members"]
        .as_array()
        .context("cargo metadata output has no workspace_members array")?
        .iter()
        .filter_map(|value| value.as_str())
        .collect();
    let packages = metadata["packages"]
        .as_array()
        .context("cargo metadata output has no packages array")?;
    for command in commands {
        let package_name = command
            .get(3)
            .context("configured feature command has no package")?;
        let feature_names = command
            .get(5)
            .context("configured feature command has no features")?;
        let package = packages
            .iter()
            .find(|package| {
                package["name"].as_str() == Some(package_name)
                    && package["id"]
                        .as_str()
                        .is_some_and(|id| workspace_members.contains(id))
            })
            .with_context(|| {
                format!("configured feature crate {package_name:?} is not a workspace member")
            })?;
        let features = package["features"].as_object().with_context(|| {
            format!("configured feature crate {package_name:?} has no feature map")
        })?;
        let defaults: std::collections::HashSet<&str> = features
            .get("default")
            .and_then(|value| value.as_array())
            .into_iter()
            .flatten()
            .filter_map(|value| value.as_str())
            .collect();
        for feature in feature_names.split(',') {
            anyhow::ensure!(
                feature != "default" && !defaults.contains(feature),
                "configured {package_name}:{feature} is a default feature, not optional coverage"
            );
            anyhow::ensure!(
                features.contains_key(feature),
                "configured feature {package_name}:{feature} is not declared"
            );
        }
    }
    Ok(())
}

/// Discover optional features one crate and one feature at a time. This is
/// intentionally not workspace-wide `--all-features`: each invocation has
/// a single attributable feature-unification boundary, and a mesh/citizen
/// feature cannot silently enable unrelated crates' runtime integrations.
fn auto_discover_feature_commands(
    dir: &Path,
    exclude_spec: Option<&str>,
    package_scope: Option<&[String]>,
) -> Result<(Vec<Vec<String>>, Vec<String>)> {
    let metadata = cargo_metadata(dir)?;
    let workspace_members: std::collections::HashSet<&str> = metadata["workspace_members"]
        .as_array()
        .context("cargo metadata output has no workspace_members array")?
        .iter()
        .filter_map(|value| value.as_str())
        .collect();
    let excludes = parse_feature_excludes(exclude_spec)?;
    let allowed: Option<std::collections::HashSet<&str>> =
        package_scope.map(|packages| packages.iter().map(String::as_str).collect());
    let packages = metadata["packages"]
        .as_array()
        .context("cargo metadata output has no packages array")?;

    let mut discovered = Vec::new();
    let mut skipped = Vec::new();
    for package in packages {
        let Some(package_id) = package["id"].as_str() else {
            continue;
        };
        if !workspace_members.contains(package_id) {
            continue;
        }
        let Some(package_name) = package["name"].as_str() else {
            continue;
        };
        if allowed
            .as_ref()
            .is_some_and(|allowed| !allowed.contains(package_name))
        {
            continue;
        }
        let Some(features) = package["features"].as_object() else {
            continue;
        };
        let defaults: std::collections::HashSet<&str> = features
            .get("default")
            .and_then(|value| value.as_array())
            .into_iter()
            .flatten()
            .filter_map(|value| value.as_str())
            .collect();
        for feature in features.keys() {
            if feature == "default" || defaults.contains(feature.as_str()) {
                continue;
            }
            let identity = format!("{package_name}:{feature}");
            // Existing workspace conventions: `_...` is a private test
            // harness and `cosmix` is a live Bus-citizen integration. Both
            // stay reachable through an explicit configured feature set,
            // and both are named in the report rather than silently omitted.
            if feature.starts_with('_') || feature == "cosmix" {
                skipped.push(format!("{identity} (built-in environment boundary)"));
                continue;
            }
            if excludes.contains(&(package_name.to_string(), feature.clone())) {
                skipped.push(format!("{identity} (FOREMAN_FEATURE_EXCLUDE)"));
                continue;
            }
            discovered.push((
                identity,
                vec![
                    "cargo".into(),
                    "test".into(),
                    "-p".into(),
                    package_name.into(),
                    "--features".into(),
                    feature.clone(),
                ],
            ));
        }
    }
    discovered.sort_by(|left, right| left.0.cmp(&right.0));
    skipped.sort();
    Ok((
        discovered.into_iter().map(|(_, command)| command).collect(),
        skipped,
    ))
}

fn cargo_metadata(dir: &Path) -> Result<serde_json::Value> {
    let cargo = crate::target_dir::trusted_cargo_from_path(dir)?;
    let output = Command::new(cargo)
        // The resolved graph is needed for task-scope reverse dependencies;
        // `--no-deps` makes `resolve` null and cannot answer that question.
        .args(["metadata", "--all-features", "--format-version=1"])
        .current_dir(dir)
        .output()
        .context("running cargo metadata for rust tier-1 feature discovery")?;
    anyhow::ensure!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    serde_json::from_slice(&output.stdout).context("parsing cargo metadata output")
}

fn parse_feature_excludes(
    spec: Option<&str>,
) -> Result<std::collections::HashSet<(String, String)>> {
    let Some(spec) = spec else {
        return Ok(Default::default());
    };
    let mut excludes = std::collections::HashSet::new();
    for (index, entry) in spec.split(";;").enumerate() {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let (package, feature) = entry.split_once(':').with_context(|| {
            format!(
                "FOREMAN_FEATURE_EXCLUDE entry #{} must be 'crate:feature'",
                index + 1
            )
        })?;
        anyhow::ensure!(
            !package.trim().is_empty() && !feature.trim().is_empty(),
            "FOREMAN_FEATURE_EXCLUDE entry #{} must name both a crate and feature",
            index + 1
        );
        excludes.insert((package.trim().to_string(), feature.trim().to_string()));
    }
    Ok(excludes)
}

/// `cosmix-comp`'s own commands (task 32): builds and lints the compositor
/// crate, INCLUDING its non-default features, from inside the `desktop/`
/// workspace `compositor`'s [`Profile::cwd`] resolves to.
///
/// Feature coverage — `cosmix-comp`'s `[features]` are `default =
/// ["frame-capture"]`, `kms-live`, `explicit-sync-live-test`:
/// - `frame-capture` (default): compiled AND tested — step 3's plain
///   `cargo test` carries it, same as any default-feature crate.
/// - `kms-live`: COMPILED, not tested unattended. Step 2's
///   `--features kms-live` clippy pass reaches the `#[cfg(all(feature =
///   "kms-live", not(test)))]` arms (the ones the crate's own default
///   `cargo clippy --all-targets` — no feature flag — never type-checks,
///   since `--all-targets` alone only turns on `cfg(test)`, and those arms
///   require the feature AND `not(test)` together). Step 4's plain
///   `cargo build --features kms-live` then compiles the real, non-test
///   binary the same way, proving the codegen path (not just clippy's
///   type-check) holds. Actually driving a KMS session needs real
///   hardware and a VT this fleet verifier does not have and must not
///   touch (see the task's constraints) — physical acceptance is a
///   separate concern (fleet task 35), not this gate's job.
/// - `explicit-sync-live-test`: COMPILED, not run, for the same reason —
///   and here "not run" is the crate's own choice, not just this profile's.
///   The feature gates 20 sites in `protocol/tests.rs`; 6 of them are
///   `#[test]`s and all 6 also carry `#[ignore = "requires
///   XDG_RUNTIME_DIR and opens the DRM render node named by
///   COSMIX_TEST_RENDER_NODE"]`, so even `cargo test --features
///   explicit-sync-live-test` would run none of them without `--ignored`
///   and a real render node. Compiling them is therefore the entire
///   available gain, and step 2's clippy pass takes it: those 20 sites
///   type-check and lint clean under `-D warnings` instead of sitting
///   uncompiled and unverified. The feature is deliberately NOT added to
///   step 3's `cargo test`, where it would add build time and change
///   nothing that runs.
///
/// `[patch.crates-io]` assertion — the `desktop/` workspace patches
/// `smithay` (and `wgpu`/`wgpu-core`) to vendored forks, and an unsatisfied
/// `[patch]` is only a cargo WARNING, so step 0 is `cargo tree -i smithay`.
/// Be precise about what that buys, because the exit status and the output
/// prove different things:
/// - EXIT STATUS: hard-fails if `smithay` has left the graph entirely
///   ("package ID specification did not match any packages"), which is the
///   total-loss case.
/// - OUTPUT: prints the resolved source — `smithay v0.7.0
///   (…/desktop/vendor/smithay)` when the patch took, a bare registry
///   version when it did not — into the verifier's captured report, where
///   an operator reading a red tier can see the provenance.
///
/// The obvious strengthening, `--locked` (a fallback to crates.io would add
/// a `source =`/`checksum` to `desktop/Cargo.lock`, and `--locked` turns any
/// lock change into a hard error) is NOT used, and deliberately: this
/// workspace path-depends on the sibling `mix` checkout, whose
/// `cosmix-lib-mix` version moves independently of this repo, so
/// `desktop/Cargo.lock` legitimately drifts (measured: the checked-in lock
/// says 0.51.0, the sibling is 0.53.0). `--locked` would make this tier red
/// on every host whose sibling checkout is not lockstep — a false failure
/// for a reason cosmix-comp does not own, which is the same class of
/// mistake as claiming coverage you do not have, pointed the other way.
///
/// TIMING — measured 2026-08-22, 18-core workstation, `-j` default, into an
/// EMPTY `CARGO_TARGET_DIR` (warm `~/.cargo` registry; `vendor/smithay` is a
/// path dep, so nothing is fetched). Per step: `tree` 0s, `fmt` 1s, `clippy`
/// 73s, `test` 144s, `build --features kms-live` 117s — 335s total. The same
/// five steps against a warm target dir: 27s total.
///
/// What matters for the cap is that [`TIER0_TIMEOUT`] is PER STEP, not per
/// tier: the number to compare against 600s is the slowest single step
/// (144s cold), not the 335s total. So this profile fits the stock tier-0
/// budget with ~4x headroom and needs no tier-0 policy override on a host of
/// this class. Two caveats an operator should carry:
/// a host with a cold `~/.cargo` also pays the crates.io download for
/// bevy/wgpu/smithay's graph on top, and a slower or more contended box
/// scales all five numbers together — if the slowest step approaches 600s,
/// raise the cap rather than let it read as a compositor failure.
fn compositor_tier_commands(
    tier: u8,
    policy: &crate::config::FleetPolicy,
) -> Result<Vec<Vec<String>>> {
    /// Scoping unit: `-p`, not `--manifest-path`. Both resolve against the
    /// same `desktop/` workspace (so the `[patch]` applies either way), but
    /// a package name cannot go stale if the crate is moved inside the
    /// workspace, whereas a hard-coded relative manifest path silently
    /// becomes a different error the day someone reorganises `crates/`.
    const PKG: &str = "cosmix-comp";
    /// The NON-DEFAULT features this profile is here to compile. Named
    /// explicitly, never `--all-features`: a blanket flag would also switch
    /// on mesh/citizen features elsewhere in `desktop/` that need a live
    /// broker (fleet task 15's conclusion). Cargo fails loudly — `error:
    /// Package cosmix-comp does not have feature ...` — if one of these is
    /// renamed or dropped, which is exactly the behaviour wanted: a feature
    /// that vanished must break the gate, not quietly stop being covered.
    const NON_DEFAULT_FEATURES: &[&str] = &["kms-live", "explicit-sync-live-test"];

    // An empty feature set would make the clippy step below identical to a
    // plain default-features run while still *reading* as non-default
    // coverage. Refuse rather than pass hollow.
    let features = NON_DEFAULT_FEATURES.join(",");
    if features.is_empty() {
        anyhow::bail!(
            "compositor profile has an empty non-default feature set: it would \
             claim kms-live coverage it does not have"
        );
    }

    let tier0 = vec![
        // 0: [patch.crates-io] smithay resolves to the vendored fork, not
        // whatever crates.io last published under that name.
        vec!["cargo".into(), "tree".into(), "-i".into(), "smithay".into()],
        vec![
            "cargo".into(),
            "fmt".into(),
            "--check".into(),
            "-p".into(),
            PKG.into(),
        ],
        // One clippy pass, both non-default features unioned: reaches the
        // `all(feature = "kms-live", not(test))` arms (via the plain
        // lib/bin target clippy builds alongside --all-targets) AND the
        // `explicit-sync-live-test`-gated test module (via the test
        // target, always cfg(test)).
        vec![
            "cargo".into(),
            "clippy".into(),
            "--all-targets".into(),
            "-p".into(),
            PKG.into(),
            "--features".into(),
            features.clone(),
            "--".into(),
            "-D".into(),
            "warnings".into(),
        ],
        // Default-feature test suite — the one thing here actually run.
        vec!["cargo".into(), "test".into(), "-p".into(), PKG.into()],
        // The real, non-test binary with kms-live on: proves the codegen
        // path compiles, not just clippy's type-check.
        vec![
            "cargo".into(),
            "build".into(),
            "-p".into(),
            PKG.into(),
            "--features".into(),
            "kms-live".into(),
        ],
    ];
    match tier {
        // Tier 1 is identical to tier 0, deliberately. The rust profile's
        // tier 1 both widens to `--workspace` tests AND adds `cargo deny
        // check`; neither widening is safe here.
        //
        // `-p cosmix-comp` already resolves inside the desktop/ workspace
        // (so the [patch] still applies) while staying scoped to that one
        // crate — going wider would mean the REST of desktop/ (trayd, mail,
        // tower, studio, filemgr, ctk, wl-dnd), which fails for unrelated
        // reasons: the exact hazard a blanket --all-features is banned
        // workspace-wide for.
        //
        // `cargo deny check` is out for a plainer reason, measured here
        // rather than assumed: the `rust` profile can run it because
        // `src/deny.toml` exists and encodes this project's policy;
        // `desktop/` has NO deny.toml at all, so cargo-deny falls back to
        // its own defaults and reports 748 license rejections plus 6
        // advisories across the workspace. Nor can it be narrowed to this
        // crate — it audits the resolved lockfile as a whole, and
        // cosmix-comp is not even innocent of the flagged graph (ttf-parser
        // reaches it via bevy → winit → sctk-adwaita → ab_glyph). Wiring it
        // in would make this tier permanently red on every host with
        // cargo-deny installed. Writing a desktop/deny.toml is real work
        // and its own decision, not a side effect of adding this profile.
        0 | 1 => Ok(tier0),
        _ => Ok(policy.tier2_argv()),
    }
}

fn binary_on_path(name: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            let p = dir.join(name);
            // Executable bit required — a non-executable file on PATH would
            // turn the tier red with a confusing spawn failure.
            std::fs::metadata(&p)
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
    })
}

/// Run a profile's tier-0 commands (the completion gate). The working
/// directory is resolved from the profile's `cwd` (if set) or the
/// invocation's `--subdir` (fallback) — see [`resolve_profile_dir`].
pub fn run_profile(profile: &str, workdir: &Path, subdir: Option<&str>) -> Result<VerifyReport> {
    let policy = default_policy()?;
    let p = builtin_profile(profile)?;
    let request = GateRequest::operator_local(0, workdir, &p, subdir, &[], &policy, None)?;
    LOCAL_GATE_RUNNER.run_gate(&request)
}

/// Private local engine behind [`LocalGateRunner`]. Keeping the old direct
/// entry point inaccessible makes a new production call site use the whole
/// request seam (and therefore carry claim, source, scope and policy) or fail
/// to compile.
fn run_tier_for_crates_with_policy(
    profile: &Profile,
    tier: u8,
    dir: &Path,
    policy: &crate::config::FleetPolicy,
    crates: &[String],
) -> Result<VerifyReport> {
    let commands =
        profile.tier_commands_for_crates_in_dir_with_policy(tier, dir, policy, crates)?;
    // `builtin_profile` already normalized "" to "rust".
    let name = profile.name.as_str();
    let timeout = policy.tier_timeout(tier)?;
    // Tier 0 keeps the pre-tier report name ("rust", "none") — persisted
    // verification JSON from earlier releases uses it.
    let report_name = if tier == 0 {
        name.to_string()
    } else {
        format!("{name}:t{tier}")
    };
    // Tier 2 (nightly): join the clone lane if `dir` sits in a repo that has
    // one (a sibling clone.lock refine or the systemd wrapper created), so a
    // nightly tier-2 cannot clone concurrently with a refine. Tier 0/1 don't
    // need it — they're fast and already serialized by host_lane().
    //
    // "Join" is literal: acquire_if_in_repo returns None when the lane is
    // already held on this run's behalf. That covers both `refine --tier 2`
    // (refine holds it for its whole run and calls straight into here, on a
    // throwaway worktree whose ../clone.lock is the same file) and the
    // flock(1)-wrapped tier-2 unit. Re-acquiring in either case is a
    // self-deadlock — see clone_lock's docs.
    let _clone_lock = if tier == 2 {
        crate::clone_lock::acquire_if_in_repo(dir)?
    } else {
        None
    };
    let opaque_steps = profile.opaque_steps(tier, commands.len())?;
    let provenance_step = if tier == 1 {
        principal_test_step(profile, &commands).map(|step| ProvenanceSelection { step, tier })
    } else {
        None
    };
    let report = run_commands_with_timeout(
        &report_name,
        &commands,
        &opaque_steps,
        dir,
        VerifyRunOptions {
            timeout,
            execution: VerifyExecution::Headless,
            retry_sccache_eperm: tier <= 1,
            policy,
            provenance: provenance_step,
        },
    )?;
    Ok(finish_headless_report(name, tier, report))
}

fn principal_test_step(profile: &Profile, commands: &[Vec<String>]) -> Option<usize> {
    if profile.name == "rust" {
        return commands.iter().position(|command| {
            command == &vec!["cargo".to_string(), "test".into(), "--workspace".into()]
        });
    }
    commands.iter().position(|command| {
        crate::target_dir::cargo_argument_index(command).is_some_and(|cargo| {
            command[cargo + 1..]
                .iter()
                .any(|arg| matches!(arg.as_str(), "test" | "bench"))
        })
    })
}

fn finish_headless_report(profile: &str, tier: u8, mut report: VerifyReport) -> VerifyReport {
    debug_assert_eq!(report.execution, VerifyExecution::Headless);
    if profile == "compositor" {
        report.uncovered = compositor_headless_uncovered(tier, report.pass);
    }
    report
}

/// The compositor headless gate's limits, attached to its report rather than
/// left in prose that can drift away from the green verdict.
fn compositor_headless_uncovered(tier: u8, pass: bool) -> Vec<UncoveredGround> {
    let compile_status = if tier <= 1 && pass {
        "compiled, not executed"
    } else if tier <= 1 {
        "not executed; compilation was not established because the headless gate failed"
    } else {
        "not executed by this headless nightly tier; compiled by compositor tiers 0/1"
    };
    vec![
        UncoveredGround {
            area: "kms-live".into(),
            status: format!(
                "{compile_status}; requires operator-requested physical acceptance on an active VT and display"
            ),
        },
        UncoveredGround {
            area: "explicit-sync-live-test".into(),
            status: format!(
                "{compile_status}; requires an operator-selected DRM render node and the ignored live tests"
            ),
        },
    ]
}

/// Construct the sole foreman-owned live KMS command. Keeping this separate
/// from [`Profile::tier_commands`] makes physical acceptance impossible to
/// select with a verifier tier number. The compositor's typed confirmation
/// interlock is mandatory here even though its lower-level CLI makes it
/// optional for direct expert use.
pub fn compositor_physical_acceptance_command(
    device: &Path,
    connector: &str,
) -> Result<Vec<String>> {
    anyhow::ensure!(
        device.is_absolute(),
        "physical KMS device must be an absolute path"
    );
    let device = device
        .to_str()
        .context("physical KMS device path must be valid UTF-8")?;
    anyhow::ensure!(
        !connector.trim().is_empty(),
        "physical KMS connector must not be empty"
    );
    Ok(vec![
        "cargo".into(),
        "run".into(),
        "--release".into(),
        "-p".into(),
        "cosmix-comp".into(),
        "--features".into(),
        "kms-live".into(),
        "--".into(),
        "kms-live".into(),
        "--device".into(),
        device.into(),
        "--connector".into(),
        connector.into(),
        "--kms-confirm".into(),
    ])
}

/// Run explicitly requested compositor acceptance against physical KMS.
///
/// This is deliberately not a verifier tier and cannot record a nightly gate
/// verdict. A caller must provide the hardware identity, a finite timeout and
/// an environment outside every HEADLESS verifier child. `run_step`'s timeout
/// owns the process tree, so a timed-out acceptance cannot leave the
/// compositor running in the background.
pub fn run_compositor_physical_acceptance(
    workdir: &Path,
    device: &Path,
    connector: &str,
    timeout: Duration,
) -> Result<VerifyReport> {
    let policy = default_policy()?;
    run_compositor_physical_acceptance_with_policy(workdir, device, connector, timeout, &policy)
}

pub fn run_compositor_physical_acceptance_with_policy(
    workdir: &Path,
    device: &Path,
    connector: &str,
    timeout: Duration,
    policy: &crate::config::FleetPolicy,
) -> Result<VerifyReport> {
    anyhow::ensure!(
        std::env::var_os(HEADLESS_ENV).is_none(),
        "physical acceptance is unavailable from a HEADLESS verifier or nightly tier; run `foreman physical-acceptance` directly from the operator's active VT"
    );
    anyhow::ensure!(
        !timeout.is_zero(),
        "physical acceptance timeout must be greater than zero"
    );
    let profile = builtin_profile("compositor")?;
    let dir = profile.resolve_cwd(workdir, None)?;
    let command = compositor_physical_acceptance_command(device, connector)?;

    run_commands_with_timeout(
        "compositor:physical-acceptance",
        &[command],
        &[false],
        &dir,
        VerifyRunOptions {
            timeout,
            execution: VerifyExecution::PhysicalAcceptance,
            retry_sccache_eperm: false,
            policy,
            provenance: None,
        },
    )
    .context("running compositor physical acceptance")
}

/// Run the optional project-manifest or fleet-local landing policy in the
/// rebased worktree. Project mode is an isolated policy domain: an omitted
/// manifest step means no gate and never falls through to fleet policy.
///
/// `FOREMAN_LANDING_GATE` is deliberately a small argv surface: whitespace
/// separates arguments and no shell interprets metacharacters. The value is
/// read from the invocation's policy snapshot, never live here after earlier
/// verification work. An unset value leaves the agentic-first default open;
/// a configured but empty value is an error for the refinery to fail closed
/// on. The gate has the tier-1 step bound regardless of the verifier tier
/// selected for the refine invocation.
pub(crate) fn run_landing_gate_with_manifest(
    dir: &Path,
    policy: &crate::config::FleetPolicy,
    manifest_gate: Option<&ProfileStep>,
    project_mode: bool,
) -> Result<Option<VerifyStep>> {
    let configured;
    let step = if let Some(step) = manifest_gate {
        step
    } else if project_mode {
        return Ok(None);
    } else {
        let Some(spec) = &policy.landing_gate.value else {
            return Ok(None);
        };
        let argv: Vec<String> = spec.split_whitespace().map(String::from).collect();
        anyhow::ensure!(
            !argv.is_empty(),
            "FOREMAN_LANDING_GATE is set but contains no command"
        );
        configured = ProfileStep {
            argv,
            opaque: false,
        };
        &configured
    };
    let argv = &step.argv;
    let pin = crate::target_dir::pinned_target_dir(dir, None)?;
    let target_dir = match crate::target_dir::ensure_isolated_with_pin_for_profile(
        argv,
        dir,
        &pin,
        step.opaque,
    )? {
        crate::target_dir::TargetDirCheck::Isolated(target_dir) => {
            anyhow::ensure!(target_dir == pin, "landing-gate target pin drifted");
            Some(target_dir)
        }
        crate::target_dir::TargetDirCheck::NotACargoProject(_)
        | crate::target_dir::TargetDirCheck::NoCargoCommand => None,
    };
    run_step(
        argv,
        dir,
        policy.tier_timeout(1)?,
        step.opaque,
        VerifyExecution::Headless,
        target_dir.as_deref(),
        false,
        true,
        false,
    )
    .map(Some)
}

fn default_policy() -> Result<crate::config::FleetPolicy> {
    crate::config::FleetPolicy::load_env_defaults()
}

/// Set in every verifier step's child environment: any foreman verify that
/// runs INSIDE a verifier step (cos's own workspace tests exercise
/// `run_profile`) is already serialized by its ancestor's lane and must not
/// re-acquire it — flock never grants a second exclusive on the same file,
/// not even to the same process, so re-acquiring is a guaranteed
/// self-deadlock that burns the whole step cap (measured live: the install
/// probe's `cargo test` died at exit 124 running foreman's own harness
/// tests, both at 600s and 2400s).
///
/// Honest boundary: delegation narrows the lane's guarantee from "one
/// verifier cargo per host" to "one per lane-holding subtree" — nested
/// verifies inside the SAME step run concurrently with each other, and are
/// bounded only by [`DEPTH_ENV`] (an earlier note called them "currently
/// unreachable"; the 2026-08-21 fork bomb reached them). Agent sessions and
/// the mayor scrub this marker at spawn so no agent subtree can inherit
/// lane-skip.
pub const LANE_HELD_ENV: &str = "FOREMAN_VERIFY_LANE_HELD";

/// Set in every unattended verifier child, including each operator-defined
/// `FOREMAN_TIER2_COMMANDS` step. The separately named physical-acceptance
/// entry point refuses while this marker is present, so a nightly command
/// cannot accidentally recurse into a VT/display takeover.
pub const HEADLESS_ENV: &str = "FOREMAN_HEADLESS_VERIFY";

/// Verifier recursion depth, set in every step's child environment
/// (ambient + 1). Lane delegation makes recursion INVISIBLE to the host
/// lane — a verify inside a verifier step skips the flock — so nothing
/// there serializes or bounds a verify whose steps spawn further verifies.
/// This counter is that bound, and it must fail closed.
///
/// Measured live 2026-08-21: an agent-written test called
/// `run_profile("rust", ..)` on the workspace containing itself, so the
/// profile's `cargo test` re-ran the test — ~6× branching per level (two
/// tests × up to three profile calls), every cargo in its own memguard
/// scope: individually capped, collectively unbounded. 840 concurrent
/// cargos, ~88 GiB anon, swap full, 94 OOM kills, dead desktop.
pub const DEPTH_ENV: &str = "FOREMAN_VERIFY_DEPTH";

/// Depths still permitted: 0 (a direct verify) and 1 (foreman's own
/// harness tests exercising the engine from inside a verifier's
/// `cargo test`). Deeper is recursion, not layering.
const MAX_VERIFY_DEPTH: u32 = 2;

/// Ambient depth: 0 outside any verifier step. A present-but-unparseable
/// marker reads as the cap, not as 0 — a corrupt value must never grant a
/// fresh recursion budget.
fn verify_depth() -> u32 {
    match std::env::var(DEPTH_ENV) {
        Ok(v) => v.trim().parse().unwrap_or(MAX_VERIFY_DEPTH),
        Err(std::env::VarError::NotUnicode(_)) => MAX_VERIFY_DEPTH,
        Err(std::env::VarError::NotPresent) => 0,
    }
}

/// The recursion gate for command-running verifies.
fn ensure_depth_budget(name: &str, depth: u32) -> Result<()> {
    anyhow::ensure!(
        depth < MAX_VERIFY_DEPTH,
        "refusing verifier {name:?}: {DEPTH_ENV}={depth} — already inside {depth} \
         verifier steps, and a deeper verify is recursion. A verify whose own steps \
         spawn verifies grows without bound (2026-08-21: 840 concurrent cargos OOMed \
         the host); tests exercising the engine should drive run_commands with cheap \
         commands, never a cargo profile on a workspace that contains them."
    );
    Ok(())
}

/// Identity stamped into a verifier lane while its flock is held.
#[derive(Debug, Default)]
struct VerifyLaneHolder {
    pid: Option<i64>,
    pid_start: Option<i64>,
    acquired_at: Option<String>,
}

impl VerifyLaneHolder {
    fn describe(&self, path: &Path) -> String {
        let kernel_holders = crate::procutil::flock_holders(path);
        if !kernel_holders.is_empty() && !self.pid.is_some_and(|pid| kernel_holders.contains(&pid))
        {
            return format!(
                "pid {} per /proc/locks (owner stamp is absent or stale)",
                kernel_holders
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        if let Some(pid) = self.pid {
            let live = if crate::procutil::owner_alive(self.pid, self.pid_start) {
                "alive"
            } else {
                "no longer running"
            };
            return match &self.acquired_at {
                Some(at) => format!("pid {pid} ({live}), acquired at {at}"),
                None => format!("pid {pid} ({live})"),
            };
        }
        if kernel_holders.is_empty() {
            "an unknown process (no owner stamp and no /proc/locks holder)".to_string()
        } else {
            format!(
                "pid {} per /proc/locks (no owner stamp)",
                kernel_holders
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    }
}

fn read_verify_lane_holder(path: &Path) -> VerifyLaneHolder {
    let mut holder = VerifyLaneHolder::default();
    let Ok(mut file) = std::fs::File::open(path) else {
        return holder;
    };
    let mut contents = String::new();
    if file.read_to_string(&mut contents).is_err() {
        return holder;
    }
    for line in contents.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "pid" => holder.pid = value.parse().ok(),
            "pid_start" => holder.pid_start = value.parse().ok(),
            "acquired_at" => holder.acquired_at = Some(value.to_string()),
            _ => {}
        }
    }
    holder
}

fn stamp_verify_lane(file: &mut std::fs::File) -> std::io::Result<()> {
    let pid = std::process::id() as i64;
    let body = format!(
        "pid={pid}\npid_start={}\nacquired_at={}\n",
        crate::procutil::starttime(pid)
            .map(|start| start.to_string())
            .unwrap_or_default(),
        chrono::Utc::now().to_rfc3339(),
    );
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(body.as_bytes())?;
    file.flush()
}

fn ancestor_lane_holder(path: &Path) -> Option<i64> {
    let me = std::process::id() as i64;
    let kernel_holders = crate::procutil::flock_holders(path);
    if let Some(pid) = kernel_holders
        .iter()
        .copied()
        .find(|pid| *pid != me && crate::procutil::process_is_ancestor(*pid, me))
    {
        return Some(pid);
    }
    // A live kernel holder that is not our ancestor wins over stale file
    // contents. Consult the stamp only when /proc could not identify the
    // current holder at all.
    if !kernel_holders.is_empty() {
        return None;
    }
    let holder = read_verify_lane_holder(path);
    holder.pid.filter(|pid| {
        *pid != me
            && crate::procutil::owner_alive(Some(*pid), holder.pid_start)
            && crate::procutil::process_is_ancestor(*pid, me)
    })
}

/// Verifier lane: one cargo verifier at a time for the selected scope. Legacy
/// runs use one lane per host; project manifests default to `verify.lock`
/// below their own root, and `FOREMAN_VERIFY_LANE` can select another private
/// path. Acquisition is bounded and names its stamped holder on failure.
/// Returns None inside a delegated verifier step — see [`LANE_HELD_ENV`].
fn host_lane(policy: &crate::config::FleetPolicy) -> Result<Option<std::fs::File>> {
    host_lane_with_delegation(policy, std::env::var_os(LANE_HELD_ENV).is_some())
}

fn host_lane_with_delegation(
    policy: &crate::config::FleetPolicy,
    delegated: bool,
) -> Result<Option<std::fs::File>> {
    use std::os::fd::AsRawFd;
    if delegated {
        return Ok(None);
    }
    let path = &policy.verify_lane.value;
    if policy.verify_lane.source == crate::config::Source::Project
        && let Some(parent) = path.parent()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating project verify lane root {}", parent.display()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let wait_secs = policy.verify_lane_wait_secs.value;
    let deadline = std::time::Instant::now() + Duration::from_secs(wait_secs);
    loop {
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            // Diagnostic only: the kernel flock remains authoritative if a
            // filesystem error prevents the owner stamp.
            let _ = stamp_verify_lane(&mut file);
            return Ok(Some(file));
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EWOULDBLOCK) {
            return Err(anyhow::Error::new(error).context(format!("locking {}", path.display())));
        }
        if policy.verify_lane.source == crate::config::Source::Default
            && let Some(pid) = ancestor_lane_holder(path)
        {
            anyhow::bail!(
                "would deadlock on the host verify lane held by pid {pid}; set FOREMAN_VERIFY_LANE to a private absolute path"
            );
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!(
                "verify lane acquisition timed out after {wait_secs}s waiting on {} — blocked on {}",
                path.display(),
                read_verify_lane_holder(path).describe(path)
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The profile-independent engine, exposed so tests can drive it with
/// arbitrary commands without paying for a cargo run.
pub fn run_commands(name: &str, commands: &[Vec<String>], dir: &Path) -> Result<VerifyReport> {
    let opaque_steps = vec![false; commands.len()];
    let policy = default_policy()?;
    run_commands_with_timeout(
        name,
        commands,
        &opaque_steps,
        dir,
        VerifyRunOptions {
            timeout: TIER0_TIMEOUT,
            execution: VerifyExecution::Headless,
            retry_sccache_eperm: false,
            policy: &policy,
            provenance: None,
        },
    )
}

struct VerifyRunOptions<'a> {
    timeout: Duration,
    execution: VerifyExecution,
    retry_sccache_eperm: bool,
    policy: &'a crate::config::FleetPolicy,
    provenance: Option<ProvenanceSelection>,
}

/// Integration-test seam for the landing-only provenance policy. Production
/// callers select it through [`run_tier_for_crates_with_policy`] at tier 1.
#[doc(hidden)]
pub fn run_commands_with_landing_provenance(
    name: &str,
    commands: &[Vec<String>],
    dir: &Path,
) -> Result<VerifyReport> {
    let opaque_steps = vec![false; commands.len()];
    let policy = default_policy()?;
    let step = commands
        .iter()
        .position(|command| {
            crate::target_dir::cargo_argument_index(command).is_some()
                && command
                    .iter()
                    .any(|arg| matches!(arg.as_str(), "test" | "bench"))
        })
        .context("landing provenance test command list has no cargo test/bench step")?;
    run_commands_with_timeout(
        name,
        commands,
        &opaque_steps,
        dir,
        VerifyRunOptions {
            timeout: TIER0_TIMEOUT,
            execution: VerifyExecution::Headless,
            retry_sccache_eperm: false,
            policy: &policy,
            provenance: Some(ProvenanceSelection { step, tier: 1 }),
        },
    )
}

#[derive(Clone, Copy)]
struct ProvenanceSelection {
    step: usize,
    tier: u8,
}

#[allow(clippy::too_many_arguments)]
fn run_commands_with_timeout(
    name: &str,
    commands: &[Vec<String>],
    opaque_steps: &[bool],
    dir: &Path,
    options: VerifyRunOptions<'_>,
) -> Result<VerifyReport> {
    let VerifyRunOptions {
        timeout,
        execution,
        retry_sccache_eperm,
        policy,
        provenance,
    } = options;
    anyhow::ensure!(
        commands.len() == opaque_steps.len(),
        "verifier {name:?} has {} commands but {} opaque-step declarations",
        commands.len(),
        opaque_steps.len()
    );
    if let Some(selection) = provenance {
        anyhow::ensure!(
            selection.step < commands.len(),
            "verifier {name:?} provenance step {} is outside {} commands",
            selection.step,
            commands.len()
        );
    }
    // Only command-running verifications contend for the host lane or spend
    // recursion budget; the empty "none" profile has nothing to serialize.
    let _lane = if commands.is_empty() {
        None
    } else {
        ensure_depth_budget(name, verify_depth())?;
        host_lane(policy)?
    };
    // Derive the same single pin the agent drivers use. The metadata probe
    // does not select a path and never executes a command wrapper; it asks a
    // trusted Cargo only whether this pin wins and the manifest resolves.
    // Every Cargo step is checked once here and again immediately before it
    // executes, closing config rewrites between steps.
    let pinned_target = crate::target_dir::pinned_target_dir(dir, None)?;
    let mut target_dir: Option<std::path::PathBuf> = None;
    for (step_index, argv) in commands.iter().enumerate() {
        // NOT `.with_context(...)`: anyhow's `Display` for a context-wrapped
        // error shows only the OUTER message, not the chain — a caller that
        // does `err.to_string()` (as the isolation-refusal regression test
        // does, and as any future caller reasonably would) would silently
        // lose the actual "Isolation cannot be established" reason from
        // `target_dir::ensure_isolated` underneath a generic wrapper.
        // Folding the source's own message into this one keeps it visible
        // to a plain `to_string()` without requiring `{:#}` or `.chain()`.
        let check = crate::target_dir::ensure_isolated_with_pin_for_profile(
            argv,
            dir,
            &pinned_target,
            opaque_steps[step_index],
        )
        .map_err(|source| {
            anyhow::anyhow!(
                "verifying cargo target-dir isolation for verifier {name:?}, command {argv:?} under {} — {source}",
                dir.display()
            )
        })?;
        match check {
            crate::target_dir::TargetDirCheck::Isolated(resolved) => {
                anyhow::ensure!(
                    resolved == pinned_target,
                    "verifier {name:?} command {argv:?} reported {} instead of its single pin {}",
                    resolved.display(),
                    pinned_target.display()
                );
                target_dir = Some(pinned_target.clone());
            }
            // Not an isolation problem — cargo itself has nothing to build
            // here (no Cargo.toml, a malformed manifest, ...). That is an
            // ordinary verifier failure, so report it as one instead of
            // aborting the run the way a proven isolation breach must.
            crate::target_dir::TargetDirCheck::NotACargoProject(msg)
                if crate::target_dir::cargo_argument_index(argv).is_some() =>
            {
                return Ok(VerifyReport {
                    profile: name.to_string(),
                    execution,
                    pass: false,
                    steps: vec![VerifyStep {
                        command: "cargo metadata".to_string(),
                        pass: false,
                        exit_code: None,
                        tail: msg,
                        annotations: Vec::new(),
                        sccache_incident: None,
                        executed_binaries: None,
                    }],
                    uncovered: Vec::new(),
                    target_dir: None,
                    provenance_tier: None,
                });
            }
            // A cheap non-cargo command may intentionally run outside a
            // Cargo project. A declared opaque command also lands here, with
            // target and binary evidence left explicitly unknown.
            crate::target_dir::TargetDirCheck::NotACargoProject(_)
            | crate::target_dir::TargetDirCheck::NoCargoCommand => {}
        }
    }
    let mut steps = Vec::new();
    let mut pass = true;
    for (step_index, argv) in commands.iter().enumerate() {
        if let Some(target_dir) = target_dir.as_deref()
            && crate::target_dir::cargo_argument_index(argv).is_some()
        {
            let check = crate::target_dir::ensure_isolated_with_pin_for_profile(
                argv,
                dir,
                target_dir,
                opaque_steps[step_index],
            )
            .map_err(|source| {
                anyhow::anyhow!(
                    "re-probing pinned Cargo target immediately before verifier {name:?}, command {argv:?} under {} — {source}",
                    dir.display()
                )
            })?;
            match check {
                crate::target_dir::TargetDirCheck::Isolated(reported) => {
                    anyhow::ensure!(
                        reported == target_dir,
                        "verifier {name:?} command {argv:?} reported {} instead of its pinned target {}",
                        reported.display(),
                        target_dir.display()
                    );
                }
                crate::target_dir::TargetDirCheck::NotACargoProject(msg) => {
                    steps.push(VerifyStep {
                        command: "cargo metadata".to_string(),
                        pass: false,
                        exit_code: None,
                        tail: msg,
                        annotations: Vec::new(),
                        sccache_incident: None,
                        executed_binaries: None,
                    });
                    pass = false;
                    break;
                }
                crate::target_dir::TargetDirCheck::NoCargoCommand => {
                    anyhow::bail!(
                        "verifier {name:?} command {argv:?} lost its Cargo token between target selection and execution"
                    );
                }
            }
        }
        let step = run_step(
            argv,
            dir,
            timeout,
            execution == VerifyExecution::Headless,
            execution,
            target_dir.as_deref(),
            opaque_steps[step_index],
            retry_sccache_eperm,
            provenance.is_some_and(|selection| selection.step == step_index),
        )
        .with_context(|| format!("running verifier step {:?}", argv.join(" ")))?;
        let ok = step.pass;
        steps.push(step);
        if !ok {
            pass = false;
            break;
        }
    }
    let provenance_tier = provenance.and_then(|selection| {
        steps
            .get(selection.step)
            .and_then(|step| step.executed_binaries.as_ref())
            .map(|_| selection.tier)
    });
    Ok(VerifyReport {
        profile: name.to_string(),
        execution,
        pass,
        steps,
        uncovered: Vec::new(),
        target_dir: target_dir.map(|path| path.to_string_lossy().into_owned()),
        provenance_tier,
    })
}

/// The workstation rule: cargo builds/tests run under `memguard` (a
/// MemoryMax systemd scope) so a runaway parallel build OOMs its own scope,
/// not the desktop. Applied whenever memguard is on PATH.
fn memguard_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path).any(|dir| dir.join("memguard").is_file())
        })
    })
}

#[derive(Clone, Copy)]
struct SccacheProbeTools<'a> {
    ss: &'a Path,
    sccache: &'a Path,
}

impl SccacheProbeTools<'static> {
    fn production() -> Self {
        Self {
            ss: Path::new("/usr/bin/ss"),
            sccache: Path::new("/usr/bin/sccache"),
        }
    }
}

struct CapturedPipe {
    tail: Vec<u8>,
    matched_sccache_eperm: bool,
    match_excerpt: Option<Vec<u8>>,
}

struct StepAttempt {
    pass: bool,
    exit_code: Option<i32>,
    stdout_tail: String,
    stderr_tail: String,
    matched_sccache_eperm: bool,
    sccache_match_excerpt: Option<String>,
    nofile: NofileLimit,
}

impl StepAttempt {
    fn labelled_tail(&self) -> String {
        match (
            self.stdout_tail.trim().is_empty(),
            self.stderr_tail.trim().is_empty(),
        ) {
            (true, true) => String::new(),
            (false, true) => self.stdout_tail.clone(),
            (true, false) => self.stderr_tail.clone(),
            (false, false) => format!(
                "--- stdout (tail) ---\n{}\n--- stderr (tail) ---\n{}",
                self.stdout_tail, self.stderr_tail
            ),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_step(
    argv: &[String],
    dir: &Path,
    timeout: Duration,
    delegates_verify_lane: bool,
    execution: VerifyExecution,
    target_dir: Option<&Path>,
    opaque: bool,
    retry_sccache_eperm: bool,
    collect_provenance: bool,
) -> Result<VerifyStep> {
    run_step_with_probe_tools(
        argv,
        dir,
        timeout,
        delegates_verify_lane,
        execution,
        target_dir,
        opaque,
        retry_sccache_eperm,
        collect_provenance,
        SccacheProbeTools::production(),
    )
}

#[allow(clippy::too_many_arguments)]
fn run_step_with_probe_tools(
    argv: &[String],
    dir: &Path,
    timeout: Duration,
    delegates_verify_lane: bool,
    execution: VerifyExecution,
    target_dir: Option<&Path>,
    opaque: bool,
    retry_sccache_eperm: bool,
    collect_provenance: bool,
    probe_tools: SccacheProbeTools<'_>,
) -> Result<VerifyStep> {
    anyhow::ensure!(!argv.is_empty(), "empty verifier command");
    if argv.first().map(String::as_str) == Some("foreman-verify-gap") {
        let reason = argv.get(1).map(String::as_str).unwrap_or("unknown");
        let subject = argv.get(2).map(String::as_str).unwrap_or("unknown");
        let detail = argv.get(3).map(String::as_str).unwrap_or_default();
        let fix = match reason {
            "crate-scope-undiscoverable" => {
                "Fix the task's crates designation or the branch's Cargo metadata."
            }
            "feature-coverage-undiscoverable" => {
                "Fix the branch's Cargo metadata, or set FOREMAN_FEATURE_SETS to explicit crate:feature entries."
            }
            _ => {
                "Fix FOREMAN_FEATURE_SETS/FOREMAN_FEATURE_EXCLUDE, or unset them to use automatic per-crate discovery."
            }
        };
        return Ok(VerifyStep {
            command: argv.join(" "),
            pass: false,
            exit_code: Some(1),
            tail: format!(
                "tier-1 verification gap: {reason}\n{subject}: {detail}\n\
                 Feature-gated code was not established. {fix}"
            ),
            annotations: Vec::new(),
            sccache_incident: None,
            executed_binaries: None,
        });
    }
    if argv.first().map(String::as_str) == Some("foreman-verify-info") {
        return Ok(VerifyStep {
            command: argv.join(" "),
            pass: true,
            exit_code: Some(0),
            tail: argv.get(2).cloned().unwrap_or_default(),
            annotations: Vec::new(),
            sccache_incident: None,
            executed_binaries: None,
        });
    }
    let child_target = match target_dir {
        Some(target_dir) => target_dir.to_path_buf(),
        None => crate::target_dir::pinned_target_dir(dir, None)?,
    };
    let child_argv = crate::target_dir::hardened_cargo_argv(argv, dir, &child_target)?;
    let deadline = std::time::Instant::now()
        .checked_add(timeout)
        .context("verifier step deadline overflowed")?;
    let use_memguard =
        crate::target_dir::cargo_argument_index(argv) == Some(0) && memguard_available();
    // A test/bench snapshot is taken BEFORE the real command. Its `--no-run`
    // compilation is the build the real Cargo command would otherwise do,
    // and it shares this step's existing deadline.
    let collect_binaries = |bypass_wrapper: bool| {
        if opaque {
            crate::provenance::BinaryProvenance::Unavailable {
                reason: "profile declared this command opaque; effective cargo argv is unknown"
                    .to_string(),
            }
        } else if crate::target_dir::cargo_argument_index(argv).is_some() {
            match target_dir {
                Some(target_dir) => crate::provenance::collect_with_rustc_wrapper(
                    argv,
                    dir,
                    target_dir,
                    deadline,
                    use_memguard,
                    bypass_wrapper,
                ),
                None => crate::provenance::BinaryProvenance::Unavailable {
                    reason: "no verified private target directory for this cargo step".to_string(),
                },
            }
        } else {
            crate::provenance::BinaryProvenance::NotApplicable
        }
    };
    let mut before_binaries = collect_provenance.then(|| collect_binaries(false));
    let mut attempt = run_step_attempt(
        argv,
        &child_argv,
        dir,
        &child_target,
        deadline,
        use_memguard,
        delegates_verify_lane,
        execution,
        false,
    )?;
    let mut annotations = Vec::new();
    let mut sccache_incident = None;
    let mut bypassed_wrapper = false;
    if !attempt.pass && attempt.matched_sccache_eperm {
        let mut incident = capture_sccache_incident(&attempt, dir, deadline, probe_tools);
        if retry_sccache_eperm {
            // The first provenance snapshot used the failed wrapper route.
            // Replace it so recovered evidence describes the wrapper-free
            // attempt that actually made the logical step green.
            before_binaries = collect_provenance.then(|| collect_binaries(true));
            attempt = run_step_attempt(
                argv,
                &child_argv,
                dir,
                &child_target,
                deadline,
                use_memguard,
                delegates_verify_lane,
                execution,
                true,
            )?;
            bypassed_wrapper = true;
            if attempt.pass {
                incident.bypass_retry = SccacheBypassRetry::Passed;
                annotations.push(SCCACHE_BYPASSED_ANNOTATION.to_string());
            } else {
                incident.bypass_retry = SccacheBypassRetry::Failed;
            }
        }
        sccache_incident = Some(incident);
    }
    let executed_binaries = before_binaries.map(|before_binaries| {
        if target_dir.is_some() {
            crate::provenance::finish_with_rustc_wrapper(
                before_binaries,
                argv,
                dir,
                &child_target,
                deadline,
                use_memguard,
                bypassed_wrapper,
            )
        } else {
            before_binaries
        }
    });
    Ok(VerifyStep {
        command: argv.join(" "),
        pass: attempt.pass,
        exit_code: attempt.exit_code,
        tail: attempt.labelled_tail(),
        annotations,
        sccache_incident,
        executed_binaries,
    })
}

#[allow(clippy::too_many_arguments)]
fn run_step_attempt(
    argv: &[String],
    child_argv: &[String],
    dir: &Path,
    child_target: &Path,
    deadline: std::time::Instant,
    use_memguard: bool,
    delegates_verify_lane: bool,
    execution: VerifyExecution,
    bypass_rustc_wrapper: bool,
) -> Result<StepAttempt> {
    let Some(remaining) = deadline
        .checked_duration_since(std::time::Instant::now())
        .filter(|remaining| !remaining.is_zero())
    else {
        return Ok(StepAttempt {
            pass: false,
            exit_code: Some(124),
            stdout_tail: String::new(),
            stderr_tail: format!(
                "verifier step deadline exhausted before {} attempt",
                if bypass_rustc_wrapper {
                    "wrapper-free retry"
                } else {
                    "initial"
                }
            ),
            matched_sccache_eperm: false,
            sccache_match_excerpt: None,
            nofile: NofileLimit {
                raw: None,
                error: Some("attempt was not spawned".to_string()),
            },
        });
    };
    // Both attempts share the original deadline: the retry does not mint a
    // second timeout budget.
    let mut cmd = Command::new("timeout");
    cmd.arg("-k")
        .arg("30")
        .arg(format!("{:.3}s", remaining.as_secs_f64().max(0.001)));
    if use_memguard {
        cmd.arg("memguard");
    }
    cmd.args(child_argv).current_dir(dir);
    if delegates_verify_lane {
        cmd.env(LANE_HELD_ENV, "1");
    } else {
        cmd.env_remove(LANE_HELD_ENV);
    }
    if execution == VerifyExecution::Headless {
        cmd.env(HEADLESS_ENV, "1");
    } else {
        cmd.env_remove(HEADLESS_ENV);
    }
    cmd.env(DEPTH_ENV, (verify_depth() + 1).to_string());
    if bypass_rustc_wrapper {
        // An explicit empty value overrides both the ambient variable and
        // Cargo configuration. Removing it could reveal configured sccache.
        cmd.env("RUSTC_WRAPPER", "");
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    crate::target_dir::pin_target_dir(&mut cmd, child_target);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning {:?}", argv[0]))?;
    let nofile = read_step_nofile(child.id());
    let out_pipe = child.stdout.take().expect("stdout piped");
    let err_pipe = child.stderr.take().expect("stderr piped");
    let out_thread = std::thread::spawn(move || rolling_tail_matching(out_pipe, None));
    let err_thread = std::thread::spawn(move || {
        rolling_tail_matching(err_pipe, Some(SCCACHE_TRIGGER.as_bytes()))
    });
    let status = child.wait().context("waiting for verifier step")?;
    let out = out_thread.join().unwrap_or(CapturedPipe {
        tail: Vec::new(),
        matched_sccache_eperm: false,
        match_excerpt: None,
    });
    let err = err_thread.join().unwrap_or(CapturedPipe {
        tail: Vec::new(),
        matched_sccache_eperm: false,
        match_excerpt: None,
    });
    Ok(StepAttempt {
        pass: status.success(),
        exit_code: status.code(),
        stdout_tail: String::from_utf8_lossy(&out.tail).into_owned(),
        stderr_tail: String::from_utf8_lossy(&err.tail).into_owned(),
        matched_sccache_eperm: err.matched_sccache_eperm,
        sccache_match_excerpt: err
            .match_excerpt
            .map(|excerpt| String::from_utf8_lossy(&excerpt).into_owned()),
        nofile,
    })
}

fn read_step_nofile(pid: u32) -> NofileLimit {
    let path = PathBuf::from(format!("/proc/{pid}/limits"));
    match read_bounded_file(&path, TAIL_BYTES) {
        Ok(contents) => {
            let raw = contents
                .lines()
                .find(|line| line.starts_with("Max open files"))
                .map(str::to_string);
            NofileLimit {
                error: raw
                    .is_none()
                    .then(|| format!("{} had no Max open files row", path.display())),
                raw,
            }
        }
        Err(error) => NofileLimit {
            raw: None,
            error: Some(error),
        },
    }
}

fn capture_sccache_incident(
    attempt: &StepAttempt,
    dir: &Path,
    deadline: std::time::Instant,
    tools: SccacheProbeTools<'_>,
) -> SccacheIncident {
    let (server_port, server_port_context) = sccache_server_port();
    let port_filter = format!("sport = :{server_port}");
    let listener_probe = run_diagnostic_probe(
        tools.ss,
        &["-H", "-tlnp", port_filter.as_str()],
        dir,
        deadline,
    );
    let server_pid = extract_ss_pid(&listener_probe.output);
    let (server_cgroup, server_cgroup_error) = match server_pid {
        Some(pid) => {
            let path = PathBuf::from(format!("/proc/{pid}/cgroup"));
            match read_bounded_file(&path, TAIL_BYTES) {
                Ok(cgroup) => (Some(cgroup), None),
                Err(error) => (None, Some(error)),
            }
        }
        None => (
            None,
            Some("listener probe did not expose an sccache pid".to_string()),
        ),
    };
    let show_stats_probe = run_diagnostic_probe(tools.sccache, &["--show-stats"], dir, deadline);
    SccacheIncident {
        original_exit_code: attempt.exit_code,
        original_stdout_tail: attempt.stdout_tail.clone(),
        original_stderr_tail: attempt.stderr_tail.clone(),
        server_port,
        server_port_context,
        listener_probe,
        server_pid,
        server_cgroup,
        server_cgroup_error,
        show_stats_probe,
        step_nofile: attempt.nofile.clone(),
        errno: attempt
            .sccache_match_excerpt
            .as_deref()
            .and_then(extract_sccache_errno_context)
            .or_else(|| extract_sccache_errno_context(&attempt.stderr_tail)),
        bypass_retry: SccacheBypassRetry::NotAttempted,
    }
}

fn sccache_server_port() -> (u16, String) {
    match std::env::var("SCCACHE_SERVER_PORT") {
        Ok(raw) => match raw.parse::<u16>() {
            Ok(port) if port != 0 => (port, "SCCACHE_SERVER_PORT".to_string()),
            Ok(_) => (
                4226,
                "default; SCCACHE_SERVER_PORT=0 is not a listener port".to_string(),
            ),
            Err(error) => (
                4226,
                format!("default; invalid SCCACHE_SERVER_PORT={raw:?}: {error}"),
            ),
        },
        Err(std::env::VarError::NotPresent) => (4226, "default".to_string()),
        Err(std::env::VarError::NotUnicode(_)) => (
            4226,
            "default; SCCACHE_SERVER_PORT was not UTF-8".to_string(),
        ),
    }
}

fn run_diagnostic_probe(
    program: &Path,
    args: &[&str],
    dir: &Path,
    deadline: std::time::Instant,
) -> DiagnosticProbe {
    let command = std::iter::once(program.to_string_lossy().into_owned())
        .chain(args.iter().map(|arg| (*arg).to_string()))
        .collect::<Vec<_>>()
        .join(" ");
    let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) else {
        return DiagnosticProbe {
            command,
            exit_code: None,
            output: "not run: verifier step deadline exhausted".to_string(),
        };
    };
    let cap = remaining.min(SCCACHE_DIAGNOSTIC_TIMEOUT);
    if cap.is_zero() {
        return DiagnosticProbe {
            command,
            exit_code: None,
            output: "not run: verifier step deadline exhausted".to_string(),
        };
    }
    let mut cmd = Command::new("timeout");
    cmd.arg("-k")
        .arg("1")
        .arg(format!("{:.3}s", cap.as_secs_f64().max(0.001)))
        .arg(program)
        .args(args)
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => {
            return DiagnosticProbe {
                command,
                exit_code: None,
                output: format!("spawn failed: {error}"),
            };
        }
    };
    let stdout = child.stdout.take().expect("diagnostic stdout piped");
    let stderr = child.stderr.take().expect("diagnostic stderr piped");
    let out_thread = std::thread::spawn(move || rolling_tail_matching(stdout, None));
    let err_thread = std::thread::spawn(move || rolling_tail_matching(stderr, None));
    let status = match child.wait() {
        Ok(status) => status,
        Err(error) => {
            return DiagnosticProbe {
                command,
                exit_code: None,
                output: format!("wait failed: {error}"),
            };
        }
    };
    let out = out_thread
        .join()
        .map(|capture| capture.tail)
        .unwrap_or_default();
    let err = err_thread
        .join()
        .map(|capture| capture.tail)
        .unwrap_or_default();
    DiagnosticProbe {
        command,
        exit_code: status.code(),
        output: labelled_output(&out, &err),
    }
}

fn extract_ss_pid(output: &str) -> Option<u32> {
    output.lines().find_map(|line| {
        if !line.contains("sccache") {
            return None;
        }
        let start = line.find("pid=")? + "pid=".len();
        let digits: String = line[start..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        (!digits.is_empty()).then(|| digits.parse().ok()).flatten()
    })
}

fn extract_errno_context(stderr: &str) -> Option<ErrnoContext> {
    let start = stderr.find("os error ")? + "os error ".len();
    let digits: String = stderr[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    let number = digits.parse::<i32>().ok()?;
    Some(ErrnoContext {
        number,
        symbol: (number == libc::EPERM).then(|| "EPERM".to_string()),
        description: std::io::Error::from_raw_os_error(number).to_string(),
    })
}

fn extract_sccache_errno_context(stderr: &str) -> Option<ErrnoContext> {
    let trigger_end = stderr.find(SCCACHE_TRIGGER)? + SCCACHE_TRIGGER.len();
    extract_errno_context(&stderr[trigger_end..])
}

fn read_bounded_file(path: &Path, cap: usize) -> std::result::Result<String, String> {
    use std::io::Read as _;
    let file = std::fs::File::open(path)
        .map_err(|error| format!("reading {}: {error}", path.display()))?;
    let mut bytes = Vec::new();
    file.take((cap + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("reading {}: {error}", path.display()))?;
    if bytes.len() > cap {
        return Err(format!(
            "{} exceeded diagnostic cap of {cap} bytes",
            path.display()
        ));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn labelled_output(stdout: &[u8], stderr: &[u8]) -> String {
    let out = String::from_utf8_lossy(stdout);
    let err = String::from_utf8_lossy(stderr);
    match (out.trim().is_empty(), err.trim().is_empty()) {
        (true, true) => String::new(),
        (false, true) => out.into_owned(),
        (true, false) => err.into_owned(),
        (false, false) => format!("--- stdout (tail) ---\n{out}\n--- stderr (tail) ---\n{err}"),
    }
}

/// Read a pipe to EOF keeping only the last [`TAIL_BYTES`], while matching a
/// fixed byte string across chunk boundaries against the complete stream.
fn rolling_tail_matching(mut pipe: impl std::io::Read, needle: Option<&[u8]>) -> CapturedPipe {
    let mut tail: Vec<u8> = Vec::new();
    let mut overlap: Vec<u8> = Vec::new();
    let mut matched = false;
    let mut match_excerpt = None;
    let mut chunk = [0u8; 4096];
    while let Ok(n) = pipe.read(&mut chunk) {
        if n == 0 {
            break;
        }
        if !matched && let Some(needle) = needle {
            let mut searchable = overlap;
            searchable.extend_from_slice(&chunk[..n]);
            if let Some(at) = searchable
                .windows(needle.len())
                .position(|window| window == needle)
            {
                matched = true;
                let end = (at + 512).min(searchable.len());
                match_excerpt = Some(searchable[at..end].to_vec());
            }
            let keep = needle.len().saturating_sub(1).min(searchable.len());
            overlap = searchable[searchable.len() - keep..].to_vec();
        } else if let Some(excerpt) = &mut match_excerpt
            && excerpt.len() < 512
        {
            let take = (512 - excerpt.len()).min(n);
            excerpt.extend_from_slice(&chunk[..take]);
        }
        tail.extend_from_slice(&chunk[..n]);
        if tail.len() > TAIL_BYTES {
            let cut = tail.len() - TAIL_BYTES;
            tail.drain(..cut);
        }
    }
    CapturedPipe {
        tail,
        matched_sccache_eperm: matched,
        match_excerpt,
    }
}

#[cfg(test)]
mod sccache_tests {
    use super::*;
    use crate::fixture::write_executable;

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        write_executable(&path, format!("#!/bin/sh\n{body}\n"));
        path
    }

    struct ProbeFixture {
        ss: PathBuf,
        sccache: PathBuf,
    }

    impl ProbeFixture {
        fn tools(&self) -> SccacheProbeTools<'_> {
            SccacheProbeTools {
                ss: &self.ss,
                sccache: &self.sccache,
            }
        }
    }

    fn probe_fixture(dir: &Path) -> ProbeFixture {
        let pid = std::process::id();
        let ss = script(
            dir,
            "fake-ss",
            &format!("echo 'LISTEN 0 128 127.0.0.1:4226 users:((\"sccache\",pid={pid},fd=8))'"),
        );
        let sccache = script(dir, "fake-sccache", "echo fixture-sccache-stats");
        ProbeFixture { ss, sccache }
    }

    fn run_fixture(
        command: &Path,
        dir: &Path,
        retry: bool,
        tools: SccacheProbeTools<'_>,
    ) -> VerifyStep {
        run_step_with_probe_tools(
            &[command.to_string_lossy().into_owned()],
            dir,
            Duration::from_secs(10),
            false,
            VerifyExecution::Headless,
            None,
            false,
            retry,
            false,
            tools,
        )
        .unwrap()
    }

    #[test]
    fn exact_stderr_eperm_retries_once_without_rustc_wrapper_and_files_info() {
        let tmp = tempfile::tempdir().unwrap();
        let count = tmp.path().join("count");
        let command = script(
            tmp.path(),
            "step",
            &format!(
                "n=$(cat '{}' 2>/dev/null || echo 0); n=$((n+1)); echo $n > '{}'; \
                 if [ $n -eq 1 ]; then \
                   echo 'sccache: error: Operation not permitted (os error 1)' >&2; \
                   i=0; while [ $i -lt 900 ]; do echo noise-$i >&2; i=$((i+1)); done; exit 2; \
                 fi; \
                 if [ -n \"$RUSTC_WRAPPER\" ]; then echo wrapper-not-empty >&2; exit 9; fi; \
                 echo retry-green",
                count.display(),
                count.display()
            ),
        );
        let probes = probe_fixture(tmp.path());
        let step = run_fixture(&command, tmp.path(), true, probes.tools());

        assert!(step.pass, "{step:?}");
        assert_eq!(std::fs::read_to_string(&count).unwrap().trim(), "2");
        assert_eq!(step.exit_code, Some(0));
        assert!(step.tail.contains("retry-green"), "{}", step.tail);
        assert_eq!(
            step.annotations,
            vec![SCCACHE_BYPASSED_ANNOTATION.to_string()]
        );
        let incident = step.sccache_incident.as_ref().expect("incident evidence");
        assert_eq!(incident.bypass_retry, SccacheBypassRetry::Passed);
        assert_eq!(incident.original_exit_code, Some(2));
        assert_eq!(incident.errno.as_ref().map(|errno| errno.number), Some(1));
        assert_eq!(
            incident
                .errno
                .as_ref()
                .and_then(|errno| errno.symbol.as_deref()),
            Some("EPERM")
        );
        assert_eq!(incident.server_pid, Some(std::process::id()));
        assert!(incident.server_cgroup.is_some(), "{incident:?}");
        assert!(
            incident
                .show_stats_probe
                .output
                .contains("fixture-sccache-stats"),
            "{incident:?}"
        );
        assert!(incident.step_nofile.raw.is_some(), "{incident:?}");

        let report = VerifyReport {
            profile: "rust".to_string(),
            execution: VerifyExecution::Headless,
            pass: true,
            steps: vec![step],
            uncovered: Vec::new(),
            target_dir: None,
            provenance_tier: None,
        };
        let db = tmp.path().join("ledger.db");
        let ledger = crate::ledger::Ledger::open(&db).unwrap();
        let task = ledger
            .add_task("sccache fixture", "spec", "impl", "low", &[], "rust")
            .unwrap();
        let ids = file_sccache_bypass_findings(&ledger, task, &report, "test").unwrap();
        assert_eq!(ids.len(), 1);
        let findings = ledger.task_findings(task).unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].1, "info");
        assert_eq!(findings[0].2, "sccache bypassed during verifier step");
        assert!(findings[0].3.contains("fixture-sccache-stats"));
    }

    #[test]
    fn failed_bypass_is_red_and_does_not_retry_twice() {
        let tmp = tempfile::tempdir().unwrap();
        let count = tmp.path().join("count");
        let command = script(
            tmp.path(),
            "step",
            &format!(
                "n=$(cat '{}' 2>/dev/null || echo 0); n=$((n+1)); echo $n > '{}'; \
                 if [ $n -eq 1 ]; then echo 'sccache: error: Operation not permitted (os error 1)' >&2; exit 2; fi; \
                 echo final-retry-failure >&2; exit 3",
                count.display(),
                count.display()
            ),
        );
        let probes = probe_fixture(tmp.path());
        let step = run_fixture(&command, tmp.path(), true, probes.tools());
        assert!(!step.pass, "{step:?}");
        assert_eq!(step.exit_code, Some(3));
        assert_eq!(std::fs::read_to_string(count).unwrap().trim(), "2");
        assert!(step.tail.contains("final-retry-failure"));
        assert!(step.annotations.is_empty());
        assert_eq!(
            step.sccache_incident.as_ref().unwrap().bypass_retry,
            SccacheBypassRetry::Failed
        );
        let report = VerifyReport {
            profile: "rust".to_string(),
            execution: VerifyExecution::Headless,
            pass: false,
            steps: vec![step],
            uncovered: Vec::new(),
            target_dir: None,
            provenance_tier: None,
        };
        let digest = report.failure_digest();
        assert!(digest.contains("final-retry-failure"), "{digest}");
        assert!(digest.contains("sccache EPERM attribution"), "{digest}");
        assert!(digest.contains("Operation not permitted"), "{digest}");
        assert!(report.sccache_bypass_digests().is_empty());
    }

    #[test]
    fn stdout_match_does_not_trigger_and_disabled_retry_only_attributes() {
        let tmp = tempfile::tempdir().unwrap();
        let stdout_count = tmp.path().join("stdout-count");
        let stdout_command = script(
            tmp.path(),
            "stdout-step",
            &format!(
                "echo x >> '{}'; echo 'sccache: error: Operation not permitted (os error 1)'; exit 2",
                stdout_count.display()
            ),
        );
        let probes = probe_fixture(tmp.path());
        let stdout_step = run_fixture(&stdout_command, tmp.path(), true, probes.tools());
        assert!(!stdout_step.pass);
        assert!(stdout_step.sccache_incident.is_none());
        assert_eq!(
            std::fs::read_to_string(stdout_count)
                .unwrap()
                .lines()
                .count(),
            1
        );

        let stderr_count = tmp.path().join("stderr-count");
        let stderr_command = script(
            tmp.path(),
            "stderr-step",
            &format!(
                "echo x >> '{}'; echo 'sccache: error: Operation not permitted (os error 1)' >&2; exit 2",
                stderr_count.display()
            ),
        );
        let stderr_step = run_fixture(&stderr_command, tmp.path(), false, probes.tools());
        assert!(!stderr_step.pass);
        assert_eq!(
            stderr_step.sccache_incident.as_ref().unwrap().bypass_retry,
            SccacheBypassRetry::NotAttempted
        );
        assert_eq!(
            std::fs::read_to_string(stderr_count)
                .unwrap()
                .lines()
                .count(),
            1
        );
    }

    #[test]
    fn persisted_step_without_new_fields_deserializes() {
        let step: VerifyStep = serde_json::from_str(
            r#"{"command":"cargo clippy","pass":false,"exit_code":2,"tail":"old","executed_binaries":{"status":"not_applicable"}}"#,
        )
        .unwrap();
        assert!(step.annotations.is_empty());
        assert!(step.sccache_incident.is_none());
    }

    #[test]
    fn parsers_extract_pid_errno_and_match_across_chunks() {
        assert_eq!(
            extract_ss_pid("users:((\"sccache\",pid=1365,fd=8))"),
            Some(1365)
        );
        let errno = extract_errno_context("Operation not permitted (os error 1)").unwrap();
        assert_eq!(errno.number, libc::EPERM);
        assert_eq!(errno.symbol.as_deref(), Some("EPERM"));

        let mut bytes = vec![b'x'; 4096 - 10];
        bytes.extend_from_slice(SCCACHE_TRIGGER.as_bytes());
        bytes.extend_from_slice(b" (os error 1)\n");
        bytes.extend(std::iter::repeat_n(b'n', TAIL_BYTES + 1));
        let capture = rolling_tail_matching(
            std::io::Cursor::new(bytes),
            Some(SCCACHE_TRIGGER.as_bytes()),
        );
        assert!(capture.matched_sccache_eperm);
        assert!(capture.tail.len() <= TAIL_BYTES);
        let excerpt = String::from_utf8(capture.match_excerpt.unwrap()).unwrap();
        assert!(excerpt.contains("os error 1"), "{excerpt}");
    }

    #[test]
    fn trigger_local_errno_wins_over_later_tail_noise() {
        let tmp = tempfile::tempdir().unwrap();
        let probes = probe_fixture(tmp.path());
        let attempt = StepAttempt {
            pass: false,
            exit_code: Some(2),
            stdout_tail: String::new(),
            stderr_tail: "later unrelated failure (os error 24)".to_string(),
            matched_sccache_eperm: true,
            sccache_match_excerpt: Some(
                "sccache: error: Operation not permitted (os error 1); later (os error 24)"
                    .to_string(),
            ),
            nofile: NofileLimit {
                raw: Some("Max open files 1024 1024 files".to_string()),
                error: None,
            },
        };
        let incident = capture_sccache_incident(
            &attempt,
            tmp.path(),
            std::time::Instant::now() + Duration::from_secs(5),
            probes.tools(),
        );
        assert_eq!(incident.errno.unwrap().number, libc::EPERM);
    }

    #[test]
    fn exhausted_deadline_does_not_spawn_an_attempt() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("spawned");
        let command = script(
            tmp.path(),
            "must-not-run",
            &format!("touch '{}'", marker.display()),
        );
        let argv = vec![command.to_string_lossy().into_owned()];
        let attempt = run_step_attempt(
            &argv,
            &argv,
            tmp.path(),
            &tmp.path().join("target"),
            std::time::Instant::now() - Duration::from_millis(1),
            false,
            false,
            VerifyExecution::Headless,
            true,
        )
        .unwrap();
        assert!(!attempt.pass);
        assert_eq!(attempt.exit_code, Some(124));
        assert!(attempt.stderr_tail.contains("deadline exhausted"));
        assert!(!marker.exists(), "expired attempts must not spawn a child");
    }
}

#[cfg(test)]
mod lane_tests {
    use super::*;

    #[test]
    fn lane_is_reentrant_by_delegation() {
        let name = "verify::lane_tests::lane_is_reentrant_by_delegation_owned_process";
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                name,
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(LANE_HELD_ENV, "1")
            .env("COSMIX_FOREMAN_VERIFY_LANE_HELPER", name)
            .output()
            .expect("spawn owned verify-lane helper process");
        assert!(
            out.status.success(),
            "owned verify-lane helper failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    #[ignore = "run only in the process spawned by lane_is_reentrant_by_delegation"]
    fn lane_is_reentrant_by_delegation_owned_process() {
        assert_eq!(
            std::env::var("COSMIX_FOREMAN_VERIFY_LANE_HELPER").as_deref(),
            Ok("verify::lane_tests::lane_is_reentrant_by_delegation_owned_process")
        );
        // Hold the real lane from this process, mark delegation, and a
        // verify must COMPLETE instead of self-deadlocking into its step
        // cap — the exact failure the install probe measured (its cargo
        // test step ran foreman's own verifying tests and died at 124).
        let temp = tempfile::tempdir().unwrap();
        let mut policy = crate::config::FleetPolicy::defaults();
        policy.verify_lane = crate::config::Sourced {
            value: temp.path().join("verify.lock"),
            source: crate::config::Source::Env,
        };
        let _held = host_lane_with_delegation(&policy, false).unwrap();
        // The parent installed the delegation marker on this owned process.
        // `run_commands` takes the production reader and must join the lane
        // already held above instead of self-deadlocking.
        let report = run_commands(
            "lane",
            &[vec!["true".to_string()]],
            std::path::Path::new("/tmp"),
        );
        let report = report.unwrap();
        assert!(report.pass, "delegated verify must run, not deadlock");
    }

    #[test]
    fn verify_lane_owner_stamp_names_this_process() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("verify.lock");
        let mut file = std::fs::File::create(&path).unwrap();
        stamp_verify_lane(&mut file).unwrap();
        let holder = read_verify_lane_holder(&path);
        assert_eq!(holder.pid, Some(std::process::id() as i64));
        assert!(holder.pid_start.is_some());
        assert!(holder.acquired_at.is_some());
    }
}

#[cfg(test)]
mod depth_tests {
    use super::*;

    #[test]
    fn budget_refuses_at_cap() {
        assert!(ensure_depth_budget("t", 0).is_ok());
        assert!(ensure_depth_budget("t", MAX_VERIFY_DEPTH - 1).is_ok());
        let err = ensure_depth_budget("t", MAX_VERIFY_DEPTH).unwrap_err();
        assert!(
            err.to_string().contains(DEPTH_ENV),
            "refusal must name the marker: {err}"
        );
        let err = ensure_depth_budget("t", MAX_VERIFY_DEPTH + 7).unwrap_err();
        assert!(
            err.to_string().contains("recursion"),
            "past-cap refuses: {err}"
        );
    }

    #[test]
    fn steps_see_incremented_depth() {
        // Deliberately NO ambient DEPTH_ENV mutation — run_commands reads it
        // and other tests run concurrently. Assert on the CHILD's view, and
        // compute the expectation from the ambient depth so the test also
        // passes when this suite itself runs inside a verifier step.
        let expect = verify_depth() + 1;
        let report = run_commands(
            "depth-probe",
            &[vec!["printenv".to_string(), DEPTH_ENV.to_string()]],
            std::path::Path::new("/tmp"),
        )
        .unwrap();
        assert!(
            report.pass,
            "a step spawned at depth {} must see {DEPTH_ENV}={expect}: {:?}",
            expect - 1,
            report.steps
        );
        assert_eq!(report.steps[0].tail.trim(), expect.to_string());
    }
}

#[cfg(test)]
mod profile_tests {
    use super::*;
    use std::fs;

    #[test]
    fn rust_profile_exists_and_commands_match_tier0() {
        let p = builtin_profile("rust").unwrap();
        assert_eq!(p.name, "rust");
        assert!(p.cwd.is_none(), "rust profile should have no explicit cwd");

        // Tier 0 should be exactly: cargo fmt --check, cargo clippy, cargo test
        let tier0 = tier_commands("rust", 0).unwrap();
        assert_eq!(tier0.len(), 3);
        assert_eq!(tier0[0], vec!["cargo", "fmt", "--check"]);
        assert_eq!(
            tier0[1],
            vec!["cargo", "clippy", "--all-targets", "--", "-D", "warnings"]
        );
        assert_eq!(tier0[2], vec!["cargo", "test"]);
    }

    #[test]
    fn none_profile_exists_and_has_no_commands() {
        let p = builtin_profile("none").unwrap();
        assert_eq!(p.name, "none");
        assert!(p.cwd.is_none());
        for tier in 0..=2 {
            assert!(tier_commands("none", tier).unwrap().is_empty());
        }
    }

    #[test]
    fn empty_string_resolves_to_rust_profile() {
        let p = builtin_profile("").unwrap();
        assert_eq!(p.name, "rust");
    }

    #[test]
    fn unknown_profile_fails() {
        let result = builtin_profile("unknown");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("unknown verifier profile"));
    }

    #[test]
    fn profile_resolve_cwd_with_explicit_directory() {
        let workdir = tempfile::tempdir().unwrap();
        let workdir_path = workdir.path();
        let subdir = workdir_path.join("src");
        fs::create_dir_all(&subdir).unwrap();

        // Create a profile with an explicit cwd
        let profile = Profile {
            name: "test".to_string(),
            cwd: Some("src".to_string()),
            source: ProfileSource::Builtin,
        };

        let resolved = profile.resolve_cwd(workdir_path, Some("other")).unwrap();
        assert_eq!(resolved, subdir, "should use profile's cwd, not fallback");
    }

    #[test]
    fn profile_resolve_cwd_falls_back_to_subdir() {
        let workdir = tempfile::tempdir().unwrap();
        let workdir_path = workdir.path();
        let subdir = workdir_path.join("build");
        fs::create_dir_all(&subdir).unwrap();

        // Profile with no explicit cwd falls back to subdir
        let profile = Profile {
            name: "rust".to_string(),
            cwd: None,
            source: ProfileSource::Builtin,
        };

        let resolved = profile.resolve_cwd(workdir_path, Some("build")).unwrap();
        assert_eq!(
            resolved,
            subdir.canonicalize().unwrap(),
            "should fall back to subdir"
        );
    }

    #[test]
    fn profile_resolve_cwd_with_no_subdir_uses_workdir() {
        let workdir = tempfile::tempdir().unwrap();
        let workdir_path = workdir.path();

        let profile = Profile {
            name: "rust".to_string(),
            cwd: None,
            source: ProfileSource::Builtin,
        };

        let resolved = profile.resolve_cwd(workdir_path, None).unwrap();
        assert_eq!(
            resolved,
            workdir_path.canonicalize().unwrap(),
            "should use workdir itself"
        );
    }

    #[test]
    fn profile_nonexistent_cwd_fails_with_path_in_message() {
        let workdir = tempfile::tempdir().unwrap();
        let workdir_path = workdir.path();

        let profile = Profile {
            name: "test".to_string(),
            cwd: Some("does-not-exist".to_string()),
            source: ProfileSource::Builtin,
        };

        let result = profile.resolve_cwd(workdir_path, None);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        // The error should mention the nonexistent path
        assert!(
            err.contains("does-not-exist"),
            "error should mention the nonexistent path"
        );
    }

    #[test]
    fn profile_cwd_escaping_worktree_is_refused() {
        let workdir = tempfile::tempdir().unwrap();
        let workdir_path = workdir.path();
        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path();

        // Create a symlink that points outside the worktree (on platforms that support it)
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink as symlink_fn;
            let symlink_path = workdir_path.join("escape-link");
            symlink_fn(outside, symlink_path.as_path()).expect("symlink creation should succeed");

            let profile = Profile {
                name: "test".to_string(),
                cwd: Some("escape-link".to_string()),
                source: ProfileSource::Builtin,
            };

            let result = profile.resolve_cwd(workdir_path, None);
            assert!(
                result.is_err(),
                "should refuse cwd that escapes via symlink"
            );
            let err = result.unwrap_err().to_string();
            assert!(
                err.contains("outside the task worktree"),
                "error should mention worktree containment: {err}"
            );
        }
    }

    #[test]
    fn profile_cwd_with_parent_traversal_is_refused() {
        let workdir = tempfile::tempdir().unwrap();
        let workdir_path = workdir.path();
        // Create the outside directory so canonicalize succeeds but containment fails
        let outside_dir = workdir_path.parent().unwrap().join("outside-traversal");
        fs::create_dir_all(&outside_dir).unwrap();

        let profile = Profile {
            name: "test".to_string(),
            cwd: Some("../outside-traversal".to_string()),
            source: ProfileSource::Builtin,
        };

        let result = profile.resolve_cwd(workdir_path, None);
        // Parent traversal MUST be refused, not contained
        assert!(
            result.is_err(),
            "parent traversal via '..' should be refused"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("outside the task worktree"),
            "error should mention worktree containment: {err}"
        );
    }

    /// The acceptance item "a task with the `rust` profile verifies exactly
    /// what it verifies today: SAME DIRECTORY" — proved against the
    /// pre-profile function itself, not against a restatement of what it
    /// used to do. Every fleet invocation shape the operator's units
    /// actually use is compared: both units pass `--subdir src`, and `.`
    /// / absent are the other two forms in the CLI.
    #[test]
    fn rust_profile_directory_is_byte_identical_to_the_pre_profile_resolver() {
        let workdir = tempfile::tempdir().unwrap();
        let root = workdir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("desktop")).unwrap();

        for subdir in [Some("src"), Some("."), Some("desktop"), None] {
            // What runner.rs computed BEFORE profiles could own a cwd.
            let before = crate::runner::resolve_verify_dir(root, subdir);
            // What every tier resolves now, for a profile with no owned cwd.
            let after = resolve_profile_dir("rust", root, subdir);
            assert_eq!(
                before.unwrap(),
                after.unwrap(),
                "rust profile must resolve identically to the old \
                 --subdir-only path for subdir {subdir:?}"
            );
        }
    }

    #[test]
    fn rust_profile_backward_compatibility() {
        // Test that the rust profile with no explicit cwd and fallback to subdir
        // produces the same results as the old behavior
        let workdir = tempfile::tempdir().unwrap();
        let workdir_path = workdir.path();
        let subdir = workdir_path.join("src");
        fs::create_dir_all(&subdir).unwrap();

        let profile = builtin_profile("rust").unwrap();
        assert_eq!(profile.name, "rust");
        assert!(
            profile.cwd.is_none(),
            "rust profile should have no explicit cwd"
        );

        // When resolved with a subdir, it should use that subdir
        let resolved = profile.resolve_cwd(workdir_path, Some("src")).unwrap();
        assert_eq!(
            resolved,
            subdir.canonicalize().unwrap(),
            "rust profile should respect subdir fallback"
        );
    }

    #[test]
    fn tier_commands_through_profile() {
        // Verify that tier_commands still works correctly through the profile system
        let rust_tier0 = tier_commands("rust", 0).unwrap();
        let none_tier0 = tier_commands("none", 0).unwrap();

        assert_eq!(rust_tier0.len(), 3);
        assert_eq!(rust_tier0[0][0], "cargo");
        assert!(
            none_tier0.is_empty(),
            "none profile should have empty tier 0"
        );

        // Empty string should resolve to rust
        let empty_tier0 = tier_commands("", 0).unwrap();
        assert_eq!(empty_tier0, rust_tier0);
    }

    /// Without a resolved verifier directory, the command-only API must
    /// preserve the old structural steps but add an explicit feature gap.
    /// The executing path always calls the directory-aware method instead.
    #[test]
    fn tier1_structure_is_tier0_with_the_test_step_replaced() {
        let tier0 = tier_commands("rust", 0).unwrap();
        let tier1 = tier_commands("rust", 1).unwrap();

        assert_eq!(&tier1[..2], &tier0[..2], "fmt and clippy carry over as-is");
        assert_eq!(tier1[2], vec!["cargo", "test", "--workspace"]);
        assert!(
            !tier1.contains(&vec!["cargo".to_string(), "test".to_string()]),
            "the crate-level test step is REPLACED, never run twice: {tier1:?}"
        );
        assert_eq!(tier1[3][0], "foreman-verify-gap");
        assert_eq!(tier1[3][1], "feature-coverage-undiscoverable");
        let expected = if binary_on_path("cargo-deny") { 5 } else { 4 };
        assert_eq!(tier1.len(), expected, "{tier1:?}");
        if expected == 5 {
            assert_eq!(tier1[4], vec!["cargo", "deny", "check"]);
        }
    }

    #[test]
    fn profile_with_explicit_cwd_runs_commands_in_that_directory() {
        // Prove "a profile carrying its own cwd runs there" — not just
        // resolve, but actual command execution in that directory: the same
        // `resolve_cwd` result `run_profile` and `run_tier`'s callers hand
        // down, spawned by the same `run_step` every verifier step goes
        // through.
        //
        // Deliberately NOT via `run_commands`: that wrapper takes the
        // host-wide flock lane, which this binary's own tier-1 parent
        // already holds for its whole run. Blocking on it here would stall
        // this thread until the tier's cap and report as exit 124 (measured
        // on this branch). The lane and the depth budget are orthogonal to
        // "does the profile's cwd reach the child process", and each has its
        // own test — `lane_tests` and `depth_tests`.
        let workdir = tempfile::tempdir().unwrap();
        let workdir_path = workdir.path();
        let target_dir = workdir_path.join("subdir");
        fs::create_dir_all(&target_dir).unwrap();

        let marker = target_dir.join("marker.txt");
        fs::write(&marker, "test").unwrap();
        // A different marker in the workdir root proves we're NOT running there.
        fs::write(workdir_path.join("root_marker.txt"), "root").unwrap();

        let profile = Profile {
            name: "test-cwd".to_string(),
            cwd: Some("subdir".to_string()),
            source: ProfileSource::Builtin,
        };

        // Resolve the profile's cwd (what run_profile/run_tier's callers do).
        let resolved_dir = profile.resolve_cwd(workdir_path, None).unwrap();

        assert_eq!(resolved_dir, target_dir.canonicalize().unwrap());

        // "test -f marker.txt" only succeeds when run inside `subdir`.
        let hit = run_step(
            &[
                "test".to_string(),
                "-f".to_string(),
                "marker.txt".to_string(),
            ],
            &resolved_dir,
            TIER0_TIMEOUT,
            true,
            VerifyExecution::Headless,
            None,
            false,
            false,
            false,
        )
        .unwrap();
        assert!(
            hit.pass,
            "command should pass when run in the profile's cwd: {} (exit {:?})",
            hit.tail, hit.exit_code
        );

        // ...and the root marker is NOT visible from there. Without this the
        // test would still pass if `current_dir` were ignored and the child
        // inherited the harness's own directory by accident.
        let miss = run_step(
            &[
                "test".to_string(),
                "-f".to_string(),
                "root_marker.txt".to_string(),
            ],
            &resolved_dir,
            TIER0_TIMEOUT,
            true,
            VerifyExecution::Headless,
            None,
            false,
            false,
            false,
        )
        .unwrap();
        assert!(
            !miss.pass,
            "the worktree root's marker must not be visible from the profile's cwd"
        );
    }

    #[test]
    fn resolve_profile_dir_honours_cwd_and_subdir_fallback() {
        // The exact function refinery.rs / `foreman verify` (tier 1/2) call
        // to find a task's verify directory — same as tier 0's run_profile.
        let workdir = tempfile::tempdir().unwrap();
        let workdir_path = workdir.path();
        let subdir = workdir_path.join("src");
        fs::create_dir_all(&subdir).unwrap();

        // Built-in profiles have no owned cwd: --subdir is the fallback,
        // unchanged from before profiles existed.
        let resolved = resolve_profile_dir("rust", workdir_path, Some("src")).unwrap();
        assert_eq!(resolved, subdir.canonicalize().unwrap());

        // Unknown profile is still refused, not silently resolved.
        assert!(resolve_profile_dir("bogus", workdir_path, Some("src")).is_err());
    }

    #[test]
    fn agent_target_subdir_matches_profile_cwd_precedence() {
        assert_eq!(
            profile_workspace_subdir("rust", Some("src")).unwrap(),
            Some("src".into())
        );
        assert_eq!(
            profile_workspace_subdir("compositor", Some("src")).unwrap(),
            Some("desktop".into())
        );
        assert_eq!(profile_workspace_subdir("none", None).unwrap(), None);
    }

    #[test]
    fn compositor_headless_report_names_what_it_did_not_execute() {
        let report =
            run_commands("compositor", &[vec!["true".to_string()]], Path::new("/tmp")).unwrap();
        let report = finish_headless_report("compositor", 0, report);

        assert_eq!(report.execution, VerifyExecution::Headless);
        assert!(report.pass);
        let kms = report
            .uncovered
            .iter()
            .find(|gap| gap.area == "kms-live")
            .expect("the report itself must name its live-KMS gap");
        assert!(
            kms.status.contains("compiled, not executed"),
            "a green headless report must not imply live execution: {kms:?}"
        );
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"execution\":\"headless\""), "{json}");
        assert!(json.contains("kms-live"), "{json}");
        assert!(json.contains("compiled, not executed"), "{json}");
    }

    #[test]
    fn historical_reports_deserialize_as_headless() {
        let report: VerifyReport =
            serde_json::from_str(r#"{"profile":"rust","pass":true,"steps":[]}"#).unwrap();
        assert_eq!(report.execution, VerifyExecution::Headless);
        assert!(report.uncovered.is_empty());
    }

    #[test]
    fn physical_acceptance_is_not_a_tier_and_forces_confirmation() {
        for tier in 0..=1 {
            let commands = tier_commands("compositor", tier).unwrap();
            assert!(
                commands
                    .iter()
                    .all(|command| !command.contains(&"--kms-confirm".to_string())),
                "headless tier {tier} must never select physical acceptance: {commands:?}"
            );
        }

        let command =
            compositor_physical_acceptance_command(Path::new("/dev/dri/card0"), "DP-1").unwrap();
        assert!(command.contains(&"--release".to_string()), "{command:?}");
        assert!(command.contains(&"kms-live".to_string()), "{command:?}");
        assert!(
            command.contains(&"--kms-confirm".to_string()),
            "the explicit physical route must force the typed takeover nonce: {command:?}"
        );
    }
}
