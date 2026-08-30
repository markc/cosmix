//! Project manifests: what turns foreman from "the harness that drives
//! Mark's cmctl/cos checkout" into an executable that can drive any git
//! repository.
//!
//! A manifest is loaded ONLY when the operator names one with the global
//! `--project <path>` flag. The manifest is authoritative for project
//! identity: an explicit repository/workdir or integration argument may
//! only repeat the manifest value, never redirect it. Other ordinary flags
//! retain their existing precedence. The ledger is likewise fixed: a
//! simultaneous `--db` must name the same path.
//! Operator
//! units that never pass `--project` run exactly the code paths they ran
//! before this module existed;
//! `manifest_verifier_default_applies_only_when_flag_omitted` in
//! `tests/project_manifest.rs` pins that down.
//!
//! Isolation decision (task 30): **one ledger per project**, not a shared
//! ledger with a repository-identity column on every row. A manifest requires
//! a project-specific `db` and never falls through task 27's shared ambient
//! ladder. Every other
//! piece of durable state a run touches — the governor STOP file,
//! `foreman.conf.mix`, the policy-settings scratch files — is anchored
//! beside that same ledger path (see `governor.rs`, `config.rs`,
//! `main.rs::launch`). Project state, worktrees, the repo clone lock
//! (`clone_lock.rs`), and the Cargo verifier lane (`verify.rs`) live below a
//! root derived from the canonical manifest filename and project name, not
//! below a configuration directory another manifest may share. A manifest's job is
//! therefore to name WHICH ledger and WHICH repo a command targets, while
//! the ledger stamps the manifest name plus the repository's root commit
//! identity as its single identity. The history identity survives a moved
//! checkout while refusing an unrelated repository with the same project
//! name. A populated unbound legacy ledger requires explicit migration —
//! `second_project_lands_end_to_end_without_touching_the_first` in
//! `tests/project_manifest.rs` proves the isolation this buys.
//!
//! Verifier profiles extend task 29's executable shape (name, owned cwd and
//! ordered argv per tier) rather than creating a parallel verifier. Manifest
//! steps pass through task 44's ordinary target-dir preflight and pinning.
//! Only an explicit per-step `opaque: true` declaration bypasses argv
//! inspection; a project manifest is operator-authored policy, the same
//! trust class as configured tier-2 commands.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use cosmix_mix::value::Value;

use crate::config::{bool_value, nonempty, string_list, string_value, u64_value};
use crate::executor::AgentKind;
use crate::verify::{Profile, ProfileStep};

/// Project instructions travel in one process argument. Refuse an oversized
/// manifest instead of accepting mandatory policy that prompt rendering would
/// later truncate.
pub const INSTRUCTION_PACK_CAP_BYTES: usize = 8 * 1024;

/// One agent lane this project accepts, and the environment variables that
/// must be set for a run to use it. An empty `credentials` list means the
/// lane needs nothing beyond what the driver already requires globally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneSpec {
    pub agent: AgentKind,
    pub credentials: Vec<String>,
}

/// The manifest-owned lane restriction passed to code paths that do not need
/// the rest of the project configuration (notably merge-review routing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectLanePolicy {
    pub name: String,
    pub lanes: Vec<LaneSpec>,
    /// Optional Git remote which receives the immutable verified integration
    /// tip. Remote delivery is authorised only by one complete, non-empty
    /// credential set from these lanes.
    pub push_remote: Option<String>,
}

impl ProjectLanePolicy {
    pub fn check_lane(
        &self,
        agent: AgentKind,
        env: impl Fn(&str) -> bool,
    ) -> std::result::Result<(), String> {
        check_lane_policy(&self.name, &self.lanes, agent, env)
    }

    /// Select the first manifest lane whose non-empty credential set is
    /// completely present. Empty credential lists never authorise a push:
    /// `Iterator::all` would otherwise make them pass vacuously.
    pub fn push_credential_names(
        &self,
        env: impl Fn(&str) -> bool,
    ) -> std::result::Result<Vec<String>, String> {
        self.lanes
            .iter()
            .find(|lane| {
                !lane.credentials.is_empty()
                    && lane.credentials.iter().all(|name| env(name.as_str()))
            })
            .map(|lane| lane.credentials.clone())
            .ok_or_else(|| {
                format!(
                    "project {:?} has no credential-ready lane for push_remote",
                    self.name
                )
            })
    }
}

#[derive(Debug, Clone)]
pub struct ProjectManifest {
    /// Canonical manifest file path, also passed to mayor-spawned MCP children.
    pub path: PathBuf,
    /// Canonical per-manifest project root. The ledger, cache, worktrees, and
    /// `clone.lock` and `verify.lock` live here rather than in a shared namespace.
    pub root: PathBuf,
    /// Instance identity: a short slug identifying this project.
    pub name: String,
    /// Stable Git repository identity: the sorted root commit set reachable
    /// from HEAD. Unlike the canonical checkout path, this survives moving a
    /// checkout while still distinguishing unrelated repositories.
    pub repo_identity: String,
    /// Required per-project ledger. A project invocation never falls through
    /// to the ambient/XDG ladder, which enforces the chosen one-ledger-per-
    /// project isolation model.
    pub db: PathBuf,
    /// Required cache root, honoured by `gc-cache` when `--dir` is absent.
    pub cache_dir: PathBuf,
    /// Repository path fallback for `--repo`/`--workdir`.
    pub repo: PathBuf,
    /// Integration branch fallback for `--integration`.
    pub integration: String,
    /// Branch-name template fallback for `--branch-template`, `"{id}"`
    /// substituted.
    pub branch_template: String,
    /// Sibling worktree directory template, `"{id}"` substituted.
    pub worktree_template: String,
    /// Verifier working-directory fallback for `--subdir`, relative to the
    /// repo root. `None` means the repo root itself.
    pub subdir: Option<String>,
    /// Default verifier profile name for tasks added without an explicit
    /// `--verifier`.
    pub verifier: String,
    /// Project-owned verifier definitions. Names may deliberately override a
    /// built-in for this manifest only.
    pub profiles: Vec<Profile>,
    /// Text spliced into every implementation and review prompt.
    pub instruction_pack: String,
    /// Optional Cargo package-manifest shape, relative to the repository
    /// root. `{crate}` names the operator-designated package component. When
    /// absent, project policy cannot map package manifests to task scope.
    pub package_manifest_template: Option<String>,
    /// Opt into the former rule that a scoped package manifest may change
    /// only its `[package]` version line. Ordinary scoped dependency edits are
    /// allowed when false (the default).
    pub restrict_manifest_edits: bool,
    /// Default pre-land verifier tier.
    pub landing_tier: u8,
    /// Whether merge-authority review is part of this project's landing gate.
    pub landing_review: bool,
    /// Optional operator-authored argv gate after the tier verifier.
    pub landing_gate: Option<ProfileStep>,
    /// Optional Git remote which receives the immutable verified integration
    /// tip after the local landing CAS.
    pub push_remote: Option<String>,
    /// Lane eligibility + per-lane required credentials. Absent entirely
    /// (no `lanes` key in the file) means every `AgentKind` is eligible with
    /// no extra credential requirement — today's unrestricted behaviour.
    pub lanes: Option<Vec<LaneSpec>>,
}

impl ProjectManifest {
    /// Load and validate a manifest from an explicit path. Strict-data Mix,
    /// same parser and error style as `foreman.conf.mix` (`config.rs`).
    pub fn load(path: &Path) -> Result<Self> {
        let value = cosmix_mix::parse_data_file(path)
            .map_err(|error| anyhow::anyhow!("parsing {}: {error}", path.display()))?;
        let Value::Map(ref entries) = value else {
            anyhow::bail!("{}: top level must be a map", path.display());
        };

        let mut name: Option<String> = None;
        let mut db: Option<PathBuf> = None;
        let mut cache_dir: Option<PathBuf> = None;
        let mut repo: Option<PathBuf> = None;
        let mut integration = "main".to_string();
        let mut branch_template = "task/{id}".to_string();
        let mut worktree_template = "task-{id}".to_string();
        let mut subdir: Option<String> = None;
        let mut verifier = "rust".to_string();
        let mut profiles = Vec::new();
        let mut instruction_pack: Option<String> = None;
        let mut package_manifest_template: Option<String> = None;
        let mut restrict_manifest_edits = false;
        let mut landing_tier = 1_u8;
        let mut landing_review = false;
        let mut landing_gate = None;
        let mut push_remote = None;
        let mut lanes: Option<Vec<LaneSpec>> = None;

        for (key, value) in entries.iter() {
            match key.as_str() {
                "name" => name = Some(nonempty(key, string_value(key, value)?.to_string())?),
                "db" => db = Some(PathBuf::from(string_value(key, value)?)),
                "cache_dir" => cache_dir = Some(PathBuf::from(string_value(key, value)?)),
                "repo" => repo = Some(PathBuf::from(string_value(key, value)?)),
                "integration" => {
                    integration = nonempty(key, string_value(key, value)?.to_string())?
                }
                "branch_template" => {
                    branch_template = nonempty(key, string_value(key, value)?.to_string())?;
                    anyhow::ensure!(
                        branch_template.contains("{id}"),
                        "config key `branch_template` must contain \"{{id}}\""
                    );
                }
                "worktree_template" => {
                    worktree_template = nonempty(key, string_value(key, value)?.to_string())?;
                    validate_worktree_template(&worktree_template)?;
                }
                "subdir" => subdir = Some(string_value(key, value)?.to_string()),
                "verifier" => {
                    verifier = nonempty(key, string_value(key, value)?.to_string())?;
                }
                "profiles" => profiles = parse_profiles(key, value)?,
                "instruction_pack" => {
                    instruction_pack = Some(nonempty(key, string_value(key, value)?.to_string())?)
                }
                "package_manifest_template" => {
                    let template = nonempty(key, string_value(key, value)?.to_string())?;
                    validate_package_manifest_template(&template)?;
                    package_manifest_template = Some(template);
                }
                "restrict_manifest_edits" => restrict_manifest_edits = bool_value(key, value)?,
                "landing_tier" => {
                    landing_tier = u8::try_from(u64_value(key, value)?)
                        .with_context(|| format!("config key `{key}` is larger than u8"))?;
                    anyhow::ensure!(landing_tier <= 2, "config key `{key}` must be 0, 1, or 2");
                }
                "landing_review" => landing_review = bool_value(key, value)?,
                "landing_gate" => landing_gate = Some(parse_step(key, value)?),
                "push_remote" => {
                    let remote = nonempty(key, string_value(key, value)?.to_string())?;
                    validate_push_remote(&remote)?;
                    push_remote = Some(remote);
                }
                "lanes" => lanes = Some(parse_lanes(key, value)?),
                _ => anyhow::bail!("unknown config key `{key}`"),
            }
        }

        let name = name.with_context(|| format!("{}: missing key `name`", path.display()))?;
        let instruction_pack = instruction_pack.with_context(|| {
            format!(
                "{}: missing required non-empty key `instruction_pack`",
                path.display()
            )
        })?;
        anyhow::ensure!(
            instruction_pack.len() <= INSTRUCTION_PACK_CAP_BYTES,
            "{}: config key instruction_pack is {} bytes; maximum is {} bytes (refusing to truncate mandatory project policy)",
            path.display(),
            instruction_pack.len(),
            INSTRUCTION_PACK_CAP_BYTES
        );
        validate_project_name(&name)?;
        let manifest_path = path
            .canonicalize()
            .with_context(|| format!("canonicalizing project manifest {}", path.display()))?;
        let manifest_parent = manifest_path
            .parent()
            .context("project manifest has no parent directory")?
            .to_path_buf();
        let manifest_stem = manifest_path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .context("project manifest filename must be UTF-8")?;
        let root_candidate = manifest_parent.join(format!(".foreman-{manifest_stem}-{name}"));
        let root = canonicalize_allow_missing(&root_candidate)?;
        anyhow::ensure!(
            root == root_candidate,
            "per-manifest root {} resolves to {}; symlinked project roots are refused",
            root_candidate.display(),
            root.display()
        );
        let repo = resolve_path(
            &manifest_parent,
            &repo.with_context(|| format!("{}: missing key `repo`", path.display()))?,
        )?
        .canonicalize()
        .with_context(|| format!("canonicalizing project repo from {}", path.display()))?;
        anyhow::ensure!(
            repo.is_dir(),
            "project repo {} is not a directory",
            repo.display()
        );
        let db = resolve_path(
            &root,
            &db.with_context(|| format!("{}: missing key `db`", path.display()))?,
        )?;
        let cache_dir = resolve_path(
            &root,
            &cache_dir.with_context(|| format!("{}: missing key `cache_dir`", path.display()))?,
        )?;
        anyhow::ensure!(
            !root.starts_with(&repo),
            "project root {} is inside managed repo {}; project state must stay outside agent-writable trees",
            root.display(),
            repo.display()
        );
        anyhow::ensure!(
            !repo.starts_with(&root),
            "managed repo {} is inside project root {}; repository and project state must not overlap",
            repo.display(),
            root.display()
        );
        anyhow::ensure!(
            !manifest_path.starts_with(&repo),
            "project manifest {} is inside managed repo {}; operator control data must stay outside agent-writable trees",
            manifest_path.display(),
            repo.display()
        );
        for (label, state_path) in [("db", &db), ("cache_dir", &cache_dir)] {
            anyhow::ensure!(
                !state_path.starts_with(&repo),
                "project `{label}` {} is inside managed repo {}; project state must stay outside agent-writable trees",
                state_path.display(),
                repo.display()
            );
            anyhow::ensure!(
                state_path.starts_with(&root),
                "project `{label}` {} escapes per-manifest root {}; project state must remain isolated",
                state_path.display(),
                root.display()
            );
        }
        let repo_identity = repository_identity(&repo)?;
        resolve_profile(&profiles, &verifier)
            .with_context(|| format!("{}: config key `verifier`", path.display()))?;
        anyhow::ensure!(
            push_remote.is_none() || lanes.is_some(),
            "config key `push_remote` requires manifest `lanes` credential policy"
        );

        Ok(ProjectManifest {
            path: manifest_path,
            root,
            name,
            repo_identity,
            db,
            cache_dir,
            repo,
            integration,
            branch_template,
            worktree_template,
            subdir,
            verifier,
            profiles,
            instruction_pack,
            package_manifest_template,
            restrict_manifest_edits,
            landing_tier,
            landing_review,
            landing_gate,
            push_remote,
            lanes,
        })
    }

    pub fn profile(&self, name: &str) -> Result<Profile> {
        resolve_profile(&self.profiles, name)
    }

    /// Whether `agent` may run against this project, and if a manifest
    /// requires credentials for it, which environment variables are
    /// missing from `env`. `Ok(())` means eligible with every required
    /// credential present.
    pub fn check_lane(
        &self,
        agent: AgentKind,
        env: impl Fn(&str) -> bool,
    ) -> std::result::Result<(), String> {
        let Some(lanes) = &self.lanes else {
            return Ok(());
        };
        check_lane_policy(&self.name, lanes, agent, env)
    }

    pub fn lane_policy(&self) -> Option<ProjectLanePolicy> {
        self.lanes.as_ref().map(|lanes| ProjectLanePolicy {
            name: self.name.clone(),
            lanes: lanes.clone(),
            push_remote: self.push_remote.clone(),
        })
    }
}

fn validate_push_remote(remote: &str) -> Result<()> {
    anyhow::ensure!(
        remote.len() <= 200
            && !remote.starts_with('-')
            && remote
                .chars()
                .all(|character| !character.is_whitespace() && !character.is_control()),
        "config key `push_remote` must be a non-option Git remote name without whitespace"
    );
    Ok(())
}

/// Identify a Git repository by its history rather than its checkout path.
/// A normal repository has one root commit; sorted multiple roots also give a
/// deterministic identity for histories merged with --allow-unrelated-histories.
fn repository_identity(repo: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(["-C"])
        .arg(repo)
        .args(["rev-list", "--max-parents=0", "HEAD"])
        .output()
        .with_context(|| format!("identifying project repo {}", repo.display()))?;
    anyhow::ensure!(
        output.status.success(),
        "identifying project repo {} from its root commit(s) failed: {}",
        repo.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let stdout = String::from_utf8(output.stdout)
        .with_context(|| format!("Git returned non-UTF-8 identity for {}", repo.display()))?;
    let mut roots = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    roots.sort_unstable();
    roots.dedup();
    anyhow::ensure!(
        !roots.is_empty(),
        "project repo {} has no commit at HEAD; initialise it before using a project manifest",
        repo.display()
    );
    anyhow::ensure!(
        roots
            .iter()
            .all(|root| root.len() == 40 && root.bytes().all(|byte| byte.is_ascii_hexdigit())),
        "Git returned an invalid root commit identity for {}",
        repo.display()
    );
    Ok(format!("git-root:{}", roots.join(",")))
}

fn validate_package_manifest_template(template: &str) -> Result<()> {
    let path = Path::new(template);
    anyhow::ensure!(
        !path.is_absolute(),
        "config key `package_manifest_template` must be relative to the repository"
    );
    anyhow::ensure!(
        path.components()
            .all(|component| matches!(component, std::path::Component::Normal(_))),
        "config key `package_manifest_template` must contain only normal path components"
    );
    anyhow::ensure!(
        path.file_name().and_then(|name| name.to_str()) == Some("Cargo.toml"),
        "config key `package_manifest_template` must end in Cargo.toml"
    );
    anyhow::ensure!(
        template.matches("{crate}").count() <= 1,
        "config key `package_manifest_template` may contain at most one `{{crate}}`"
    );
    anyhow::ensure!(
        !template.replace("{crate}", "").contains(['{', '}']),
        "config key `package_manifest_template` contains an unknown placeholder"
    );
    if template.contains("{crate}") {
        anyhow::ensure!(
            path.components()
                .any(|component| component.as_os_str() == "{crate}"),
            "config key `package_manifest_template` requires `{{crate}}` as a complete path component"
        );
    }
    Ok(())
}

/// Credential variables must contain a value. Treating `TOKEN=` as present
/// turns an operator typo into an authorised lane that can only fail later.
pub fn credential_in_environment(name: &str) -> bool {
    credential_value_is_nonempty(std::env::var_os(name).as_deref())
}

fn credential_value_is_nonempty(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|value| !value.is_empty())
}

fn check_lane_policy(
    project_name: &str,
    lanes: &[LaneSpec],
    agent: AgentKind,
    env: impl Fn(&str) -> bool,
) -> std::result::Result<(), String> {
    let Some(spec) = lanes.iter().find(|l| l.agent == agent) else {
        return Err(format!(
            "project {:?} does not list {} as an eligible lane",
            project_name,
            agent.as_str()
        ));
    };
    let missing: Vec<&str> = spec
        .credentials
        .iter()
        .map(String::as_str)
        .filter(|var| !env(var))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "project {:?} requires {} for {} (missing: {})",
            project_name,
            spec.credentials.join(", "),
            agent.as_str(),
            missing.join(", ")
        ))
    }
}

fn resolve_profile(profiles: &[Profile], name: &str) -> Result<Profile> {
    profiles
        .iter()
        .find(|profile| profile.name == name)
        .cloned()
        .map(Ok)
        .unwrap_or_else(|| crate::verify::lookup_profile(name))
}

fn resolve_path(base: &Path, configured: &Path) -> Result<PathBuf> {
    anyhow::ensure!(
        !configured
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir)),
        "project path {} must not contain `..`",
        configured.display()
    );
    let path = if configured.is_absolute() {
        configured.to_path_buf()
    } else {
        base.join(configured)
    };
    canonicalize_allow_missing(&path)
}

/// Canonicalise every existing component, including the final component when
/// it is a symlink, while still allowing fresh state paths. Manifest loading
/// remains read-only, so missing suffixes are appended to the first existing
/// canonical ancestor instead of being created here.
fn canonicalize_allow_missing(path: &Path) -> Result<PathBuf> {
    let mut cursor = path;
    let mut missing = Vec::new();
    loop {
        match cursor.canonicalize() {
            Ok(mut canonical) => {
                for component in missing.iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = cursor
                    .file_name()
                    .context("project path must name a file or directory")?;
                missing.push(name.to_os_string());
                cursor = cursor
                    .parent()
                    .context("project path has no existing parent directory")?;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("canonicalizing project path {}", path.display()));
            }
        }
    }
}

fn validate_project_name(name: &str) -> Result<()> {
    anyhow::ensure!(
        name.len() <= 64
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "config key `name` must be a 1-64 byte ASCII slug (letters, digits, `-`, `_`)"
    );
    Ok(())
}

fn validate_worktree_template(template: &str) -> Result<()> {
    anyhow::ensure!(
        template.contains("{id}"),
        "config key `worktree_template` must contain \"{{id}}\""
    );
    let path = Path::new(template);
    anyhow::ensure!(
        path.components().count() == 1 && !path.is_absolute(),
        "config key `worktree_template` must be one relative path component"
    );
    Ok(())
}

fn parse_profiles(key: &str, value: &Value) -> Result<Vec<Profile>> {
    let Value::Map(entries) = value else {
        anyhow::bail!("config key `{key}` must be a map of profile names to settings");
    };
    let mut profiles = Vec::with_capacity(entries.len());
    for (name, settings) in entries.iter() {
        let name = nonempty(&format!("{key}.{name}"), name.clone())?;
        anyhow::ensure!(
            !profiles
                .iter()
                .any(|profile: &Profile| profile.name == name),
            "config key `{key}` defines profile {name:?} more than once"
        );
        let Value::Map(settings) = settings else {
            anyhow::bail!("config key `{key}.{name}` must be a map");
        };
        let mut cwd = None;
        let mut tiers: [Option<Vec<ProfileStep>>; 3] = [None, None, None];
        for (field, value) in settings.iter() {
            match field.as_str() {
                "cwd" => cwd = Some(string_value(&format!("{key}.{name}.cwd"), value)?.into()),
                "tier0" | "tier1" | "tier2" => {
                    let tier = usize::from(field.as_bytes()[4] - b'0');
                    tiers[tier] = Some(parse_steps(&format!("{key}.{name}.{field}"), value)?);
                }
                _ => anyhow::bail!("unknown config key `{key}.{name}.{field}`"),
            }
        }
        let [tier0, tier1, tier2] = tiers;
        let tiers = [
            tier0.with_context(|| format!("missing config key `{key}.{name}.tier0`"))?,
            tier1.with_context(|| format!("missing config key `{key}.{name}.tier1`"))?,
            tier2.with_context(|| format!("missing config key `{key}.{name}.tier2`"))?,
        ];
        profiles.push(Profile::manifest(name, cwd, tiers));
    }
    Ok(profiles)
}

fn parse_steps(key: &str, value: &Value) -> Result<Vec<ProfileStep>> {
    let Value::List(steps) = value else {
        anyhow::bail!("config key `{key}` must be a list of argv steps");
    };
    steps
        .iter()
        .enumerate()
        .map(|(index, value)| parse_step(&format!("{key}[{index}]"), value))
        .collect()
}

fn parse_step(key: &str, value: &Value) -> Result<ProfileStep> {
    if matches!(value, Value::List(_)) {
        let argv = string_list(key, value)?;
        anyhow::ensure!(!argv.is_empty(), "config key `{key}` has empty argv");
        return Ok(ProfileStep {
            argv,
            opaque: false,
        });
    }
    let Value::Map(fields) = value else {
        anyhow::bail!("config key `{key}` must be an argv list or a step map");
    };
    let mut argv = None;
    let mut opaque = false;
    for (field, value) in fields.iter() {
        match field.as_str() {
            "argv" => argv = Some(string_list(&format!("{key}.argv"), value)?),
            "opaque" => opaque = bool_value(&format!("{key}.opaque"), value)?,
            _ => anyhow::bail!("unknown config key `{key}.{field}`"),
        }
    }
    let argv = argv.with_context(|| format!("missing config key `{key}.argv`"))?;
    anyhow::ensure!(
        !argv.is_empty(),
        "config key `{key}.argv` must not be empty"
    );
    anyhow::ensure!(
        argv.iter().all(|arg| !arg.contains('\0')),
        "config key `{key}.argv` contains NUL"
    );
    Ok(ProfileStep { argv, opaque })
}

fn parse_lanes(key: &str, value: &Value) -> Result<Vec<LaneSpec>> {
    let Value::Map(entries) = value else {
        anyhow::bail!("config key `{key}` must be a map of agent name to lane settings");
    };
    let mut out: Vec<LaneSpec> = Vec::with_capacity(entries.len());
    for (agent_name, settings) in entries.iter() {
        let agent = agent_name
            .parse::<AgentKind>()
            .map_err(anyhow::Error::msg)
            .with_context(|| format!("config key `{key}.{agent_name}`"))?;
        anyhow::ensure!(
            !out.iter().any(|l| l.agent == agent),
            "config key `{key}` lists {agent_name} more than once"
        );
        let Value::Map(settings) = settings else {
            anyhow::bail!("config key `{key}.{agent_name}` must be a map");
        };
        let mut credentials = Vec::new();
        for (sub_key, sub_value) in settings.iter() {
            match sub_key.as_str() {
                "credentials" => {
                    credentials = string_list(&format!("{key}.{agent_name}.{sub_key}"), sub_value)?;
                    anyhow::ensure!(
                        credentials.iter().all(|name| valid_environment_name(name)),
                        "config key `{key}.{agent_name}.{sub_key}` contains an invalid environment variable name"
                    );
                }
                _ => anyhow::bail!("unknown config key `{key}.{agent_name}.{sub_key}`"),
            }
        }
        out.push(LaneSpec { agent, credentials });
    }
    Ok(out)
}

fn valid_environment_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|first| first == b'_' || first.is_ascii_alphabetic())
        && bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    fn complete(dir: &Path, extra: &str) -> PathBuf {
        std::fs::create_dir_all(dir.join("repo")).unwrap();
        let repo = dir.join("repo");
        let init = Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(init.status.success());
        let commit = Command::new("git")
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--allow-empty",
                "-q",
                "-m",
                "initial",
            ])
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(commit.status.success());
        std::fs::create_dir_all(dir.join("state")).unwrap();
        std::fs::create_dir_all(dir.join("cache")).unwrap();
        write(
            dir,
            "p.mix",
            &format!(
                "name: \"demo\"\nrepo: \"repo\"\ndb: \"state/ledger.db\"\ncache_dir: \"cache\"\ninstruction_pack: \"Project rules.\"\n{extra}"
            ),
        )
    }

    #[test]
    fn empty_credential_value_is_absent() {
        assert!(!credential_value_is_nonempty(None));
        assert!(!credential_value_is_nonempty(Some(std::ffi::OsStr::new(
            ""
        ))));
        assert!(credential_value_is_nonempty(Some(std::ffi::OsStr::new(
            "token"
        ))));
    }

    #[test]
    fn minimal_manifest_gets_documented_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = complete(dir.path(), "");
        let m = ProjectManifest::load(&path).unwrap();
        assert_eq!(m.name, "demo");
        assert_eq!(m.repo, dir.path().join("repo").canonicalize().unwrap());
        assert_eq!(m.root, dir.path().join(".foreman-p-demo"));
        assert_eq!(m.db, m.root.join("state/ledger.db"));
        assert_eq!(m.cache_dir, m.root.join("cache"));
        assert_eq!(m.integration, "main");
        assert_eq!(m.branch_template, "task/{id}");
        assert_eq!(m.worktree_template, "task-{id}");
        assert_eq!(m.verifier, "rust");
        assert_eq!(m.landing_tier, 1);
        assert!(!m.landing_review);
        assert!(m.push_remote.is_none());
        assert_eq!(m.instruction_pack, "Project rules.");
        assert!(m.package_manifest_template.is_none());
        assert!(!m.restrict_manifest_edits);
        assert!(m.lanes.is_none());
        assert!(m.check_lane(AgentKind::Claude, |_| false).is_ok());
    }

    #[test]
    fn manifests_in_one_directory_have_distinct_state_and_worktree_namespaces() {
        let dir = tempfile::tempdir().unwrap();
        let first_path = complete(dir.path(), "");
        let second_path = dir.path().join("other.mix");
        std::fs::copy(&first_path, &second_path).unwrap();

        let first = ProjectManifest::load(&first_path).unwrap();
        let second = ProjectManifest::load(&second_path).unwrap();
        assert_ne!(first.root, second.root);
        assert_ne!(first.db, second.db);
        assert_ne!(first.cache_dir, second.cache_dir);
        assert_ne!(
            first.root.join("clone.lock"),
            second.root.join("clone.lock")
        );
        assert_ne!(
            first
                .root
                .join(first.worktree_template.replace("{id}", "1")),
            second
                .root
                .join(second.worktree_template.replace("{id}", "1"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn final_component_state_symlink_into_repo_is_refused() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let path = complete(dir.path(), "");
        let root = dir.path().join(".foreman-p-demo");
        let state = root.join("state");
        std::fs::create_dir_all(&state).unwrap();
        let target = dir.path().join("repo/agent-writable.db");
        std::fs::write(&target, "").unwrap();
        symlink(&target, state.join("ledger.db")).unwrap();

        let error = ProjectManifest::load(&path).unwrap_err();
        assert!(
            format!("{error:#}").contains("inside managed repo"),
            "{error:#}"
        );
    }

    #[test]
    fn missing_name_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "p.mix",
            "repo: \"/tmp/demo\"\ninstruction_pack: \"Project rules.\"\n",
        );
        let err = ProjectManifest::load(&path).unwrap_err();
        assert!(err.to_string().contains("missing key `name`"), "{err}");
    }

    #[test]
    fn missing_repo_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "p.mix",
            "name: \"demo\"\ninstruction_pack: \"Project rules.\"\n",
        );
        let err = ProjectManifest::load(&path).unwrap_err();
        assert!(err.to_string().contains("missing key `repo`"), "{err}");
    }

    #[test]
    fn missing_or_empty_instruction_pack_is_rejected_at_load() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("repo")).unwrap();
        for body in [
            "name: \"demo\"\nrepo: \"repo\"\ndb: \"ledger.db\"\ncache_dir: \"cache\"\n",
            "name: \"demo\"\nrepo: \"repo\"\ndb: \"ledger.db\"\ncache_dir: \"cache\"\ninstruction_pack: \"  \"\n",
        ] {
            let path = write(dir.path(), "p.mix", body);
            let error = ProjectManifest::load(&path).unwrap_err();
            assert!(
                format!("{error:#}").contains("instruction_pack")
                    && format!("{error:#}").contains("empty"),
                "{error:#}"
            );
        }
    }

    #[test]
    fn oversized_instruction_pack_is_rejected_instead_of_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let pack = "p".repeat(INSTRUCTION_PACK_CAP_BYTES + 1);
        let path = complete(dir.path(), "");
        let body = std::fs::read_to_string(&path)
            .unwrap()
            .replace("Project rules.", &pack);
        std::fs::write(&path, body).unwrap();
        let error = ProjectManifest::load(&path).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("maximum"), "{message}");
        assert!(message.contains("refusing to truncate"), "{message}");
    }

    #[test]
    fn unknown_verifier_fails_at_load_time() {
        let dir = tempfile::tempdir().unwrap();
        let path = complete(dir.path(), "verifier: \"nonsense\"\n");
        let err = ProjectManifest::load(&path).unwrap_err();
        assert!(
            format!("{err:#}").contains("unknown verifier profile"),
            "{err:#}"
        );
    }

    #[test]
    fn branch_template_without_id_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "p.mix",
            "name: \"demo\"\nrepo: \"/tmp/demo\"\ninstruction_pack: \"Project rules.\"\nbranch_template: \"fixed\"\n",
        );
        let err = ProjectManifest::load(&path).unwrap_err();
        assert!(err.to_string().contains("{id}"), "{err}");
    }

    #[test]
    fn package_manifest_template_is_relative_and_structured() {
        let dir = tempfile::tempdir().unwrap();
        for template in [
            "/Cargo.toml",
            "../Cargo.toml",
            "crates/prefix-{crate}/Cargo.toml",
            "crates/{package}/Cargo.toml",
            "crates/{crate}/manifest.toml",
        ] {
            let path = complete(
                dir.path(),
                &format!("package_manifest_template: {template:?}\n"),
            );
            let error = ProjectManifest::load(&path).unwrap_err();
            assert!(
                format!("{error:#}").contains("package_manifest_template"),
                "template {template:?}: {error:#}"
            );
        }
        let path = complete(
            dir.path(),
            "package_manifest_template: \"src/crates/{crate}/Cargo.toml\"\n",
        );
        assert_eq!(
            ProjectManifest::load(&path)
                .unwrap()
                .package_manifest_template
                .as_deref(),
            Some("src/crates/{crate}/Cargo.toml")
        );
    }

    #[test]
    fn restrictive_manifest_policy_is_opt_in() {
        let dir = tempfile::tempdir().unwrap();
        let path = complete(dir.path(), "restrict_manifest_edits: true\n");
        assert!(
            ProjectManifest::load(&path)
                .unwrap()
                .restrict_manifest_edits
        );
    }

    #[test]
    fn lanes_restrict_eligibility_and_require_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let path = complete(
            dir.path(),
            "lanes: {\n\
             \x20\x20claude: { credentials: [\"ANTHROPIC_API_KEY\"] },\n\
             \x20\x20codex: {}\n\
             }\n",
        );
        let m = ProjectManifest::load(&path).unwrap();
        assert!(m.check_lane(AgentKind::Glm, |_| true).is_err());
        assert!(m.check_lane(AgentKind::Codex, |_| false).is_ok());
        assert!(m.check_lane(AgentKind::Claude, |_| false).is_err());
        assert!(
            m.check_lane(AgentKind::Claude, |v| v == "ANTHROPIC_API_KEY")
                .is_ok()
        );
    }

    #[test]
    fn parses_manifest_profile_and_landing_policy() {
        let dir = tempfile::tempdir().unwrap();
        let path = complete(
            dir.path(),
            "verifier: \"project\"\n\
             landing_tier: 0\n\
             landing_review: true\n\
             landing_gate: { argv: [\"sh\", \"-c\", \"exit 0\"], opaque: true }\n\
             profiles: { project: { cwd: \".\",\n\
             \x20\x20tier0: [[\"cargo\", \"check\"]],\n\
             \x20\x20tier1: [],\n\
             \x20\x20tier2: [{ argv: [\"sh\", \"-c\", \"echo nightly\"], opaque: true }]\n\
             } }\n",
        );
        let manifest = ProjectManifest::load(&path).unwrap();
        assert_eq!(manifest.landing_tier, 0);
        assert!(manifest.landing_review);
        assert!(manifest.landing_gate.as_ref().unwrap().opaque);
        let profile = manifest.profile("project").unwrap();
        assert_eq!(profile.cwd.as_deref(), Some("."));
        assert_eq!(
            profile.tier_commands(0).unwrap(),
            vec![vec!["cargo".to_string(), "check".to_string()]]
        );
        assert!(profile.tier_commands(1).unwrap().is_empty());
    }

    #[test]
    fn push_remote_requires_lane_credentials_and_is_carried_with_policy() {
        let dir = tempfile::tempdir().unwrap();
        let path = complete(dir.path(), "push_remote: \"publish\"\n");
        let error = ProjectManifest::load(&path).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("requires manifest `lanes`"), "{message}");

        let path = complete(
            dir.path(),
            "push_remote: \"publish\"\nlanes: { codex: { credentials: [\"PUBLISH_TOKEN\"] } }\n",
        );
        let manifest = ProjectManifest::load(&path).unwrap();
        assert_eq!(manifest.push_remote.as_deref(), Some("publish"));
        let policy = manifest.lane_policy().unwrap();
        assert_eq!(policy.push_remote.as_deref(), Some("publish"));
        assert!(policy.push_credential_names(|_| false).is_err());
        assert_eq!(
            policy
                .push_credential_names(|name| name == "PUBLISH_TOKEN")
                .unwrap(),
            vec!["PUBLISH_TOKEN"]
        );
    }

    #[test]
    fn push_remote_empty_lane_credentials_never_authorise() {
        let dir = tempfile::tempdir().unwrap();
        let path = complete(
            dir.path(),
            "push_remote: \"publish\"\nlanes: { codex: { credentials: [] } }\n",
        );
        let policy = ProjectManifest::load(&path).unwrap().lane_policy().unwrap();
        assert!(
            policy.push_credential_names(|_| true).is_err(),
            "an empty credential list has nothing to check and must not authorise remote delivery"
        );
    }

    #[test]
    fn push_remote_and_credential_names_are_safe_argv_and_environment_inputs() {
        let dir = tempfile::tempdir().unwrap();
        for remote in ["--delete", "two words"] {
            let path = complete(
                dir.path(),
                &format!(
                    "push_remote: {remote:?}\nlanes: {{ codex: {{ credentials: [\"TOKEN\"] }} }}\n"
                ),
            );
            let message = format!("{:#}", ProjectManifest::load(&path).unwrap_err());
            assert!(message.contains("non-option Git remote name"), "{message}");
        }

        let path = complete(
            dir.path(),
            "lanes: { codex: { credentials: [\"BAD=TOKEN\"] } }\n",
        );
        let message = format!("{:#}", ProjectManifest::load(&path).unwrap_err());
        assert!(
            message.contains("invalid environment variable name"),
            "{message}"
        );
    }

    #[test]
    fn project_db_is_required() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("repo")).unwrap();
        std::fs::create_dir(dir.path().join("cache")).unwrap();
        let path = write(
            dir.path(),
            "p.mix",
            "name: \"demo\"\nrepo: \"repo\"\ncache_dir: \"cache\"\ninstruction_pack: \"Project rules.\"\n",
        );
        let error = ProjectManifest::load(&path).unwrap_err();
        assert!(error.to_string().contains("missing key `db`"), "{error:#}");
    }

    #[test]
    fn project_root_inside_managed_repo_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        std::fs::create_dir(dir.path().join("state")).unwrap();
        std::fs::create_dir(dir.path().join("cache")).unwrap();
        let path = write(
            &repo,
            "project.mix",
            &format!(
                "name: \"demo\"\nrepo: \"{}\"\ndb: \"{}\"\ncache_dir: \"{}\"\ninstruction_pack: \"Project rules.\"\n",
                repo.display(),
                dir.path().join("state/ledger.db").display(),
                dir.path().join("cache").display()
            ),
        );
        let error = ProjectManifest::load(&path).unwrap_err();
        assert!(
            format!("{error:#}").contains("project root")
                && format!("{error:#}").contains("inside managed repo"),
            "{error:#}"
        );
    }

    #[test]
    fn managed_repo_inside_project_root_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".foreman-p-demo");
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let path = write(
            dir.path(),
            "p.mix",
            &format!(
                "name: \"demo\"\nrepo: \"{}\"\ndb: \"ledger.db\"\ncache_dir: \"cache\"\ninstruction_pack: \"Project rules.\"\n",
                repo.display()
            ),
        );
        let error = ProjectManifest::load(&path).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("managed repo") && message.contains("inside project root"),
            "{message}"
        );
    }

    #[test]
    fn db_and_cache_inside_managed_repo_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        for (label, db, cache_dir) in [
            ("db", repo.join("ledger.db"), PathBuf::from("cache")),
            ("cache_dir", PathBuf::from("ledger.db"), repo.join("cache")),
        ] {
            let path = write(
                dir.path(),
                "p.mix",
                &format!(
                    "name: \"demo\"\nrepo: \"{}\"\ndb: \"{}\"\ncache_dir: \"{}\"\ninstruction_pack: \"Project rules.\"\n",
                    repo.display(),
                    db.display(),
                    cache_dir.display()
                ),
            );
            let error = ProjectManifest::load(&path).unwrap_err();
            let message = format!("{error:#}");
            assert!(
                message.contains(&format!("project `{label}`"))
                    && message.contains("inside managed repo"),
                "{message}"
            );
        }
    }

    #[test]
    fn unknown_key_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "p.mix",
            "name: \"demo\"\nrepo: \"/tmp/demo\"\ninstruction_pack: \"Project rules.\"\nbogus: \"x\"\n",
        );
        let err = ProjectManifest::load(&path).unwrap_err();
        assert!(
            err.to_string().contains("unknown config key `bogus`"),
            "{err}"
        );
    }
}
