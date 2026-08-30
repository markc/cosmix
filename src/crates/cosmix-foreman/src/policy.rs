//! The policy gate: rules applied to an agent session's tool calls, wired in
//! as a Claude Code PreToolUse command hook (`foreman policy-check`). The
//! hook contract is the documented stable one: tool-call JSON on stdin, exit
//! 0 = allow, exit 2 = deny with the reason on stderr (fed back to the
//! agent).
//!
//! Agentic-first (2026-08-16 law): the gate itself is OPT-IN per run
//! (`foreman run --policy`) — but once on, the rules below are the
//! machine-checked kind that bind correctness, not human ceremony:
//!
//! - An agent must never modify its own gates (this policy module, the
//!   hook settings file, the ledger, the STOP file — NOT the whole foreman
//!   crate, which is legitimate fleet work; see GATE_PATHS).
//! - Writes stay inside the task worktree (for the canonicalized edit
//!   tools; Bash is text-checked only — see the boundary below).
//! - `git push` is denied except to the task's own branch in the shared fleet
//!   repo; force-push and every push to `origin` are denied everywhere.
//! - Task refs on `origin` are never pull/rebase/reset inputs. The integration
//!   base and task-ref shape come from the active project manifest, or retain
//!   the legacy `main` / `task/{id}` values when no manifest is active.
//! - Escalation-class actions such as recursive delete are denied with a
//!   finding filed for the operator — deny-and-record is the unattended
//!   analogue of "page the mayor".
//! - A task may edit the manifest of a crate it already changes or the
//!   operator explicitly designated. This includes dependency add, remove,
//!   and re-pin edits. Operators may opt a project back into the former
//!   version-only manifest fence; the matching source-less workspace package
//!   version in `Cargo.lock` may follow an allowed version bump.
//!   Every tool is judged from the resulting content against `HEAD`; when the
//!   hook cannot reconstruct that content it fails closed.
//!   Shell write targets containing glob, expansion, or escape syntax are
//!   likewise unresolvable and fail closed, even when the literal Cargo file
//!   name is hidden by that syntax; read-only shell globs remain permitted.
//!   Scope comes from either a committed non-manifest file the task branch
//!   changes in that crate, or an operator-owned `task add --crate`
//!   designation (needed for bump-only work). Uncommitted work cannot widen
//!   scope, and free-form task title/spec prose is deliberately ignored as
//!   policy authority. See the [operator runbook] for the complete procedure.
//!
//! [operator runbook]: https://github.com/markc/cosmix/blob/main/docs/cos/foreman.md
//! - provider=zai sessions cannot read secret-bearing paths (Chinese data
//!   law applies to the Z.ai endpoint).
//!
//! Enforcement boundary: file-path rules canonicalize through symlinks, but
//! the Bash rules are TEXT analysis of a shell command — sound against
//! drift, not against an adversarial agent (quoting/concat tricks get
//! through by construction). The optional bwrap view restricts filesystem
//! visibility and cross-lane credential reads, but deliberately re-exposes
//! the writable ledger needed by this hook. It therefore does not subsume
//! these gate-path rules, and it remains default-off until separately soaked.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Internal sentinel for the legacy no-manifest layout: a package manifest
/// may sit below any workspace prefix when its tail is
/// `crates/<name>/Cargo.toml`.
pub const LEGACY_PACKAGE_MANIFEST_TEMPLATE: &str = "*/crates/{crate}/Cargo.toml";

/// How `--policy` is actually enforced for a lane.
///
/// `--policy` names an INTENT ("contain this agent"), not a mechanism. The
/// hook gate is a claude-CLI feature, so a lane without a hook surface has
/// to be contained some other way — and a lane that is refused outright
/// cannot join the ladder at all, which is what kept codex off it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyMechanism {
    /// The claude PreToolUse hook gate: per-tool-call verdicts.
    HookGate,
    /// The driver's own OS sandbox. Writes are confined to the worktree and
    /// there are no per-call verdicts. The optional bwrap layer additionally
    /// restricts reads when `FOREMAN_SANDBOX=bwrap` is selected.
    OwnSandbox,
}

/// Which mechanism contains this lane. Lifted out of `launch()` so it can
/// be tested — the binary itself has no test harness, and the previous
/// hard refusal of codex shipped with no test at all.
pub fn policy_mechanism(kind: crate::executor::AgentKind) -> PolicyMechanism {
    match kind {
        // Both drive the claude CLI, so both get the hook gate.
        crate::executor::AgentKind::Claude | crate::executor::AgentKind::Glm => {
            PolicyMechanism::HookGate
        }
        crate::executor::AgentKind::Codex => PolicyMechanism::OwnSandbox,
    }
}

/// What a rule decides. Escalate denies too — there is no human in the loop
/// mid-run — but is additionally recorded as a blocker finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Deny(String),
    Escalate(String),
}

/// Per-session context baked into the hook invocation by the runner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyContext {
    pub task_id: i64,
    /// The task worktree writes must stay inside (canonicalized).
    pub worktree: PathBuf,
    /// The only branch `git push` may target (from the task row), if any.
    pub branch: Option<String>,
    /// "zai" tightens read rules; anything else is the default provider.
    pub provider: String,
    /// Exact integration base resolved by the run caller before the hook is
    /// installed. A commit id keeps branch-scope decisions stable throughout
    /// the session even when the configured integration branch moves.
    pub integration_base: String,
    /// Operator-selected integration branch, used in policy diagnostics.
    pub integration_branch: String,
    /// Operator-selected task branch shape. `{id}` matches one task id.
    pub task_ref_template: String,
    /// Operator-selected Cargo package-manifest shape relative to the
    /// worktree. `None` leaves package manifests outside automatic task
    /// scope because their crate cannot be identified.
    pub package_manifest_template: Option<String>,
    /// Restore the former version-only fence for scoped package manifests.
    /// Default false: same-UID agents may make ordinary manifest edits in a
    /// crate already authorised by [`crate_is_task_scoped`].
    #[serde(default)]
    pub restrict_manifest_edits: bool,
    /// Crates explicitly designated by the operator when the task was added.
    /// [`run_check`] loads these from the gate-protected ledger rather than
    /// trusting free-form task prose or agent-controlled hook input.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub task_crates: Vec<String>,
}

/// Path fragments an agent must never write to: its own gates. The hook
/// settings file is protected by NAME, not just by living in .foreman/ —
/// the ledger dir is configurable.
///
/// Deliberately narrow — the tamper-evidence rail, not the whole foreman
/// crate: the fleet's own backlog IS foreman work (recursive
/// self-improvement is the mandate), so a whole-crate fragment made every
/// foreman task impossible under --policy (measured 2026-08-18: an opus
/// run burned its budget discovering it couldn't edit the crate it was
/// tasked to extend).
///
/// Honest boundary (Phase 2's, restated): these rules are a DRIFT RAIL for
/// cooperating agents — Write/Edit paths are canonicalized, but Bash
/// checks are text-level and evadable by construction (cd-splitting,
/// traversal; hard links can even launder a second name for this file's
/// inode past every path check), and enforcement code exists outside this
/// file (the PolicyCheck wiring, verifier, governor). What actually holds
/// regardless of agent cooperation: the merge-authority review — the
/// refinery makes it MANDATORY for any landing diff touching the foreman
/// crate, opt-in elsewhere — and binary deploy being an operator/installer
/// act, so source edits alone change nothing the running fleet enforces.
/// The optional bwrap view closes unbound filesystem paths even for
/// adversarial shell code, but its explicit writable ledger mount is still
/// reachable from the payload. This rail remains necessary and deliberately
/// independent of that namespace boundary.
const GATE_PATHS: &[&str] = &[
    "cosmix-foreman/src/policy.rs",
    ".foreman/",
    "ledger.db",
    "/STOP",
    "policy-settings-",
];

/// Secret-bearing path fragments provider=zai may not read. Deliberately
/// concrete (no bare "secret"/"private" — those match ordinary source
/// paths like src/private_api.rs and deny legitimate work).
const ZAI_SECRET_PATHS: &[&str] = &[
    ".env",
    "secrets/",
    "id_rsa",
    "id_ed25519",
    ".ssh/",
    ".aws/",
    ".pgpass",
    ".netrc",
    "credentials.json",
];

/// Evaluate one PreToolUse payload (the documented hook stdin JSON:
/// `tool_name` + `tool_input`).
pub fn evaluate(ctx: &PolicyContext, payload: &Value) -> Verdict {
    let tool = payload
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let input = payload.get("tool_input").cloned().unwrap_or(Value::Null);

    match tool {
        "Write" | "Edit" | "NotebookEdit" => {
            let path = input
                .get("file_path")
                .or_else(|| input.get("notebook_path"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let target = match resolve_write_target(ctx, path) {
                Ok(target) => target,
                Err(verdict) => return verdict,
            };
            if is_controlled_cargo_file(&target) {
                let proposed = match proposed_edit_content(tool, &input, &target) {
                    Ok(content) => content,
                    Err(reason) => {
                        return Verdict::Escalate(format!(
                            "edit to {path}: {reason}; proposed manifest content is unavailable, \
                             so dependency/manifest review fails closed"
                        ));
                    }
                };
                check_controlled_cargo_edit(ctx, &target, &proposed)
            } else {
                Verdict::Allow
            }
        }
        "Read" | "Grep" | "Glob" => {
            if ctx.provider == "zai" {
                // Check EVERY location field — Glob supplies both `path`
                // and `pattern`, and either alone can name the secret.
                for field in ["file_path", "path", "pattern"] {
                    if let Some(loc) = input.get(field).and_then(Value::as_str)
                        && let deny @ Verdict::Deny(_) = check_zai_read(loc)
                    {
                        return deny;
                    }
                }
                Verdict::Allow
            } else {
                Verdict::Allow
            }
        }
        "Bash" => {
            let command = input.get("command").and_then(Value::as_str).unwrap_or("");
            check_bash(ctx, command)
        }
        // Unknown tools (MCP tools, future additions) pass — the gate stops
        // known-dangerous shapes, it is not an allowlist.
        _ => Verdict::Allow,
    }
}

fn is_gate_path(path: &str) -> bool {
    GATE_PATHS.iter().any(|g| path.contains(g))
}

/// The text as the gate must read it: with the task worktree's own prefix
/// removed, so fragments match what a path means INSIDE the worktree.
///
/// Load-bearing, not cosmetic. The fleet home is literally `.foreman/` and
/// every task worktree lives inside it, so matching raw absolute paths
/// denied EVERY legitimate in-worktree write and every Bash command that
/// named an absolute path — leaving agents only relative-path shell
/// editing, the one lane this gate cannot check. Measured live 2026-08-18,
/// the day `--policy` went on: the gate inverted its own purpose.
fn gate_view(text: &str, worktree: &Path) -> String {
    let mut out = text.to_string();
    let canon = worktree
        .canonicalize()
        .unwrap_or_else(|_| worktree.to_path_buf());
    for wt in [worktree.to_path_buf(), canon] {
        // A root-equivalent prefix would blank every path, and a gate that
        // matches nothing is worse than no gate. Counted by COMPONENTS,
        // not bytes: "//" is root with a length of two.
        if wt.components().count() <= 1 {
            continue;
        }
        out = strip_worktree(&out, &wt.to_string_lossy());
    }
    out
}

/// Remove worktree-prefix occurrences from `text`, but only where the
/// prefix genuinely names THIS worktree and the remainder genuinely stays
/// inside it. Both conditions are load-bearing, and a naive
/// `str::replace` fails each one:
///
/// - **Component boundary.** `<fleet>/task-5` is a string-prefix of the
///   SIBLING `<fleet>/task-50`; blind replacement would erase that
///   sibling's `.foreman/` protection.
/// - **No lexical escape.** `<worktree>/../workdir` is not inside the
///   worktree at all — it names the integration clone, which no name
///   fragment protects. Such a path keeps its full form so `.foreman/`
///   still bites.
fn strip_worktree(text: &str, prefix: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(prefix) {
        let (before, from) = rest.split_at(at);
        let after = &from[prefix.len()..];
        // A boundary is anything that cannot continue the same filename
        // component: end of text, a separator, or shell punctuation.
        let boundary = !after.starts_with(|c: char| {
            c.is_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '~'
        });
        // ANY `..` component in the path that follows disqualifies the
        // strip — `<wt>/src/../../workdir` and `<wt>/./../workdir` land
        // outside just as surely as `<wt>/..`. Only the path-ish run is
        // examined (up to whitespace or shell punctuation) so an ordinary
        // `cd <wt> && …` still strips.
        let path_run = after
            .split(|c: char| c.is_whitespace() || "\"'`;&|<>()".contains(c))
            .next()
            .unwrap_or("");
        let escapes = path_run.split('/').any(|seg| seg == "..");
        out.push_str(before);
        if boundary && !escapes {
            // Inside this worktree: drop the prefix so fragments read the
            // path as the agent means it.
        } else {
            out.push_str(prefix);
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

fn resolve_write_target(ctx: &PolicyContext, path: &str) -> Result<PathBuf, Verdict> {
    if path.is_empty() {
        return Err(Verdict::Deny("write with no target path".into()));
    }
    if is_gate_path(&gate_view(path, &ctx.worktree)) {
        return Err(Verdict::Deny(format!(
            "{path} is part of foreman's own gates — agents never modify their gates; \
             file a finding instead"
        )));
    }
    // Containment through SYMLINKS, not just lexically: resolve the leaf if
    // it is a (possibly dangling) symlink, then canonicalize the nearest
    // existing ancestor (the file itself may not exist yet) — a
    // `safe -> /outside` link inside the worktree cannot smuggle a write
    // out. The worktree itself is canonicalized for the comparison.
    let p = Path::new(path);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        ctx.worktree.join(p)
    };
    let wt = ctx
        .worktree
        .canonicalize()
        .unwrap_or_else(|_| ctx.worktree.clone());
    let Some(resolved) = resolve_leaf_symlink(&normalize(&abs)) else {
        // A symlink chain we could not fully resolve fails CLOSED — an
        // unresolvable target is exactly where an escape would hide.
        return Err(Verdict::Deny(format!(
            "{path} is a symlink chain too deep to resolve — denied"
        )));
    };
    let normal = canonicalize_deepest(&resolved);
    // Gate check AGAIN on the resolved path: an in-worktree alias pointing
    // at a gate file passes the raw-string check above.
    if is_gate_path(&gate_view(&normal.to_string_lossy(), &wt)) {
        return Err(Verdict::Deny(format!(
            "{path} resolves into foreman's own gates — agents never modify their gates"
        )));
    }
    if !normal.starts_with(&wt) {
        return Err(Verdict::Deny(format!(
            "{path} is outside the task worktree {} — task work stays in the worktree; \
             file a finding for anything discovered elsewhere",
            ctx.worktree.display()
        )));
    }
    Ok(normal)
}

fn is_controlled_cargo_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, "Cargo.toml" | "Cargo.lock"))
}

/// Reconstruct the content the edit tool would leave on disk. The policy is
/// intentionally coupled only to the documented Write/Edit fields; an unknown
/// tool shape is not guessed at because scope is enforced against the exact
/// manifest bytes the tool would leave behind.
fn proposed_edit_content(tool: &str, input: &Value, target: &Path) -> Result<String, String> {
    match tool {
        "Write" => input
            .get("content")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| "Write payload has no string `content` field".into()),
        "Edit" => {
            let old = input
                .get("old_string")
                .and_then(Value::as_str)
                .ok_or_else(|| "Edit payload has no string `old_string` field".to_string())?;
            let new = input
                .get("new_string")
                .and_then(Value::as_str)
                .ok_or_else(|| "Edit payload has no string `new_string` field".to_string())?;
            if old.is_empty() {
                return Err("Edit `old_string` is empty".into());
            }
            let current = std::fs::read_to_string(target)
                .map_err(|e| format!("cannot read the current target: {e}"))?;
            let matches = current.matches(old).count();
            let replace_all = input
                .get("replace_all")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if matches == 0 {
                return Err("Edit `old_string` does not occur in the current target".into());
            }
            if !replace_all && matches != 1 {
                return Err(format!(
                    "Edit `old_string` occurs {matches} times and the resulting edit is ambiguous"
                ));
            }
            Ok(if replace_all {
                current.replace(old, new)
            } else {
                current.replacen(old, new, 1)
            })
        }
        _ => Err(format!(
            "{tool} does not expose a supported whole-file result for this policy"
        )),
    }
}

fn check_controlled_cargo_edit(ctx: &PolicyContext, target: &Path, proposed: &str) -> Verdict {
    let result = match target.file_name().and_then(|name| name.to_str()) {
        Some("Cargo.toml") => validate_manifest_edit(ctx, target, proposed),
        Some("Cargo.lock") => validate_lock_edit(ctx, target, proposed),
        _ => Ok(()),
    };
    match result {
        Ok(()) => Verdict::Allow,
        Err(reason) => Verdict::Escalate(format!(
            "edit to {}: {reason} — manifest policy refused the proposed content",
            target.display()
        )),
    }
}

fn validate_manifest_edit(
    ctx: &PolicyContext,
    target: &Path,
    proposed: &str,
) -> Result<(), String> {
    let (crate_name, crate_dir) = crate_for_manifest(ctx, target).ok_or_else(|| {
        match &ctx.package_manifest_template {
            Some(template) => format!(
                "only a manifest matching project template {template:?} can be mapped to task crate scope"
            ),
            None => "this project defines no package_manifest_template; manifest crate scope is unknown".to_string(),
        }
    })?;
    if !crate_is_task_scoped(ctx, &crate_name, &crate_dir) {
        return Err(format!(
            "crate {crate_name:?} is unrelated to this task (not operator-designated and no committed crate file differs from the task branch base); commit your change to {crate_name} first or have the operator designate the crate"
        ));
    }
    let head = head_content(ctx, target)?;
    validate_package_identity(&head, proposed)?;
    if ctx.restrict_manifest_edits {
        let (old, new) = changed_version_line(&head, proposed)?;
        validate_version_step(&old, &new)?;
    }
    Ok(())
}

/// A crate-scoped manifest edit may change its build definition, not turn the
/// authorised path into a different package. Parsing both complete files also
/// makes the policy decision explicitly depend on the proposed bytes rather
/// than the editing command's shape.
fn validate_package_identity(head: &str, proposed: &str) -> Result<(), String> {
    let package_name = |label: &str, content: &str| {
        let document = content
            .parse::<toml_edit::DocumentMut>()
            .map_err(|error| format!("{label} manifest is not valid TOML: {error}"))?;
        document
            .get("package")
            .and_then(|item| item.get("name"))
            .and_then(toml_edit::Item::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("{label} manifest has no string `[package].name`"))
    };
    let old = package_name("HEAD", head)?;
    let new = package_name("proposed", proposed)?;
    if new != old {
        return Err(format!(
            "proposed manifest changes package identity from {old:?} to {new:?}"
        ));
    }
    Ok(())
}

fn validate_lock_edit(ctx: &PolicyContext, target: &Path, proposed: &str) -> Result<(), String> {
    let head = head_content(ctx, target)?;
    let (changed_index, old_line, new_line) = one_replaced_line(&head, proposed)?;
    let old = parse_version_assignment(old_line)
        .ok_or_else(|| "the only Cargo.lock change is not a `version = \"…\"` line".to_string())?;
    let new = parse_version_assignment(new_line)
        .ok_or_else(|| "the only Cargo.lock change is not a `version = \"…\"` line".to_string())?;
    validate_version_step(&old, &new)?;

    let crate_name = lock_workspace_package_name(proposed, changed_index).ok_or_else(|| {
        "the changed lock version is not inside one unambiguous source-less workspace [[package]] block"
            .to_string()
    })?;
    let manifest = manifest_for_crate(ctx, target, &crate_name).ok_or_else(|| {
        "this project defines no package manifest path for the changed lock package".to_string()
    })?;
    if !manifest.is_file() {
        return Err(format!(
            "the changed lock package {crate_name:?} has no matching manifest at {}",
            manifest.display()
        ));
    }
    let (_, crate_dir) = crate_for_manifest(ctx, &manifest)
        .ok_or_else(|| "the matching crate manifest is outside the task worktree".to_string())?;
    if !crate_is_task_scoped(ctx, &crate_name, &crate_dir) {
        return Err(format!(
            "lock package {crate_name:?} is unrelated to this task"
        ));
    }

    let current_manifest = std::fs::read_to_string(&manifest)
        .map_err(|e| format!("cannot read {}: {e}", manifest.display()))?;
    let manifest_version = package_version(&current_manifest)?;
    if manifest_version != new {
        return Err(format!(
            "lock version {new:?} does not match the crate manifest version {manifest_version:?}"
        ));
    }
    Ok(())
}

fn changed_version_line(head: &str, proposed: &str) -> Result<(String, String), String> {
    let (index, old_line, new_line) = one_replaced_line(head, proposed)?;
    if table_at_line(head, index).as_deref() != Some("package")
        || table_at_line(proposed, index).as_deref() != Some("package")
    {
        return Err("the changed version line is not in `[package]`".into());
    }
    let old = parse_version_assignment(old_line)
        .ok_or_else(|| "the removed line is not the `[package]` `version` key".to_string())?;
    let new = parse_version_assignment(new_line)
        .ok_or_else(|| "the added line is not the `[package]` `version` key".to_string())?;
    Ok((old, new))
}

/// Require a literal one-line replacement. `split_inclusive` keeps line-ending
/// and key-reordering changes visible instead of normalising them away.
fn one_replaced_line<'a>(
    head: &'a str,
    proposed: &'a str,
) -> Result<(usize, &'a str, &'a str), String> {
    let old_lines: Vec<&str> = head.split_inclusive('\n').collect();
    let new_lines: Vec<&str> = proposed.split_inclusive('\n').collect();
    if old_lines.len() != new_lines.len() {
        return Err("the diff against HEAD is not one replaced line".into());
    }
    let changed: Vec<usize> = old_lines
        .iter()
        .zip(&new_lines)
        .enumerate()
        .filter_map(|(index, (old, new))| (old != new).then_some(index))
        .collect();
    if changed.len() != 1 {
        return Err(format!(
            "the diff against HEAD changes {} lines, not exactly one",
            changed.len()
        ));
    }
    let index = changed[0];
    Ok((index, old_lines[index], new_lines[index]))
}

fn table_at_line(content: &str, line_index: usize) -> Option<String> {
    let mut table = None;
    for line in content.split_inclusive('\n').take(line_index + 1) {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            table = Some(trimmed.trim_matches(['[', ']']).to_string());
        }
    }
    table
}

fn parse_version_assignment(line: &str) -> Option<String> {
    let line = line.trim_end_matches(['\r', '\n']).trim();
    let (key, value) = line.split_once('=')?;
    if key.trim() != "version" {
        return None;
    }
    let value = value.trim();
    if value.len() < 2 || !value.starts_with('"') || !value.ends_with('"') {
        return None;
    }
    let version = &value[1..value.len() - 1];
    PolicyVersion::parse(version).map(|_| version.to_string())
}

fn package_version(content: &str) -> Result<String, String> {
    let versions: Vec<_> = content
        .split_inclusive('\n')
        .enumerate()
        .filter(|(index, _)| table_at_line(content, *index).as_deref() == Some("package"))
        .filter_map(|(_, line)| parse_version_assignment(line))
        .collect();
    match versions.as_slice() {
        [version] => Ok(version.clone()),
        [] => Err("matching manifest has no explicit semver `[package]` version".into()),
        _ => Err("matching manifest has more than one `[package]` version".into()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PolicyVersion {
    major: u64,
    minor: u64,
    patch: u64,
    pre: Vec<VersionIdentifier>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum VersionIdentifier {
    Numeric(u64),
    Text(String),
}

impl PolicyVersion {
    fn parse(value: &str) -> Option<Self> {
        let (without_build, build) = value
            .split_once('+')
            .map_or((value, None), |(left, right)| (left, Some(right)));
        if build.is_some_and(|part| !valid_identifiers(part, false)) {
            return None;
        }
        let (core, pre) = without_build
            .split_once('-')
            .map_or((without_build, None), |(left, right)| (left, Some(right)));
        let mut core = core.split('.');
        let major = parse_core_number(core.next()?)?;
        let minor = parse_core_number(core.next()?)?;
        let patch = parse_core_number(core.next()?)?;
        if core.next().is_some() {
            return None;
        }
        let pre = match pre {
            Some(value) if valid_identifiers(value, true) => value
                .split('.')
                .map(|part| {
                    if part.bytes().all(|byte| byte.is_ascii_digit()) {
                        part.parse::<u64>().map(VersionIdentifier::Numeric)
                    } else {
                        Ok(VersionIdentifier::Text(part.to_string()))
                    }
                })
                .collect::<Result<Vec<_>, _>>()
                .ok()?,
            Some(_) => return None,
            None => Vec::new(),
        };
        Some(Self {
            major,
            minor,
            patch,
            pre,
        })
    }
}

impl Ord for PolicyVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| compare_prerelease(&self.pre, &other.pre))
    }
}

impl PartialOrd for PolicyVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn compare_prerelease(left: &[VersionIdentifier], right: &[VersionIdentifier]) -> Ordering {
    match (left.is_empty(), right.is_empty()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        (false, false) => {}
    }
    for (left, right) in left.iter().zip(right) {
        let order = match (left, right) {
            (VersionIdentifier::Numeric(left), VersionIdentifier::Numeric(right)) => {
                left.cmp(right)
            }
            (VersionIdentifier::Numeric(_), VersionIdentifier::Text(_)) => Ordering::Less,
            (VersionIdentifier::Text(_), VersionIdentifier::Numeric(_)) => Ordering::Greater,
            (VersionIdentifier::Text(left), VersionIdentifier::Text(right)) => left.cmp(right),
        };
        if order != Ordering::Equal {
            return order;
        }
    }
    left.len().cmp(&right.len())
}

fn parse_core_number(value: &str) -> Option<u64> {
    if value.is_empty()
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return None;
    }
    value.parse().ok()
}

fn valid_identifiers(value: &str, numeric_leading_zero_matters: bool) -> bool {
    !value.is_empty()
        && value.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && (!numeric_leading_zero_matters
                    || !part.bytes().all(|byte| byte.is_ascii_digit())
                    || part.len() == 1
                    || !part.starts_with('0'))
        })
}

fn validate_version_step(old: &str, new: &str) -> Result<(), String> {
    let old =
        PolicyVersion::parse(old).ok_or_else(|| format!("HEAD version {old:?} is not semver"))?;
    let new = PolicyVersion::parse(new)
        .ok_or_else(|| format!("proposed version {new:?} is not semver"))?;
    if new <= old {
        return Err("the proposed version is not strictly greater than HEAD".into());
    }
    if new.major != old.major {
        return Err(format!(
            "the proposed version crosses major {} to {}; major jumps stay operator-only",
            old.major, new.major
        ));
    }
    Ok(())
}

fn crate_for_manifest(ctx: &PolicyContext, manifest: &Path) -> Option<(String, PathBuf)> {
    let worktree = ctx
        .worktree
        .canonicalize()
        .unwrap_or_else(|_| ctx.worktree.clone());
    let relative = manifest.strip_prefix(&worktree).ok()?;
    let template = ctx.package_manifest_template.as_deref()?;
    let crate_name = match_manifest_template(relative, template).or_else(|| {
        (!template.contains("{crate}"))
            .then(|| std::fs::read_to_string(manifest).ok())
            .flatten()
            .and_then(|content| package_name(&content))
    })?;
    let crate_dir = manifest.parent()?.to_path_buf();
    Some((crate_name, crate_dir))
}

fn match_manifest_template(relative: &Path, template: &str) -> Option<String> {
    let relative: Vec<_> = relative.components().collect();
    if template == LEGACY_PACKAGE_MANIFEST_TEMPLATE {
        return (relative.len() >= 3
            && relative[relative.len() - 3].as_os_str() == "crates"
            && relative[relative.len() - 1].as_os_str() == "Cargo.toml")
            .then(|| {
                relative[relative.len() - 2]
                    .as_os_str()
                    .to_str()
                    .map(str::to_string)
            })
            .flatten();
    }
    let template: Vec<_> = Path::new(template).components().collect();
    if relative.len() != template.len() {
        return None;
    }
    let mut crate_name = None;
    for (actual, expected) in relative.iter().zip(template) {
        if expected.as_os_str() == "{crate}" {
            crate_name = actual.as_os_str().to_str().map(str::to_string);
        } else if actual != &expected {
            return None;
        }
    }
    crate_name
}

fn manifest_for_crate(ctx: &PolicyContext, lock: &Path, crate_name: &str) -> Option<PathBuf> {
    let template = ctx.package_manifest_template.as_deref()?;
    if template == LEGACY_PACKAGE_MANIFEST_TEMPLATE {
        return Some(
            lock.parent()?
                .join("crates")
                .join(crate_name)
                .join("Cargo.toml"),
        );
    }
    Some(ctx.worktree.join(template.replace("{crate}", crate_name)))
}

fn package_name(content: &str) -> Option<String> {
    content
        .split_inclusive('\n')
        .enumerate()
        .filter(|(index, _)| table_at_line(content, *index).as_deref() == Some("package"))
        .find_map(|(_, line)| {
            let (key, value) = line.trim().split_once('=')?;
            (key.trim() == "name").then(|| value.trim().trim_matches('"').to_string())
        })
        .filter(|name| !name.is_empty())
}

fn crate_is_task_scoped(ctx: &PolicyContext, crate_name: &str, crate_dir: &Path) -> bool {
    ctx.task_crates.iter().any(|name| name == crate_name)
        || crate_has_other_task_changes(ctx, crate_dir)
}

fn crate_has_other_task_changes(ctx: &PolicyContext, crate_dir: &Path) -> bool {
    if ctx.integration_base.is_empty() {
        return false;
    }
    let Ok(relative) = crate_dir.strip_prefix(
        ctx.worktree
            .canonicalize()
            .unwrap_or_else(|_| ctx.worktree.clone()),
    ) else {
        return false;
    };
    let Some(relative) = relative.to_str() else {
        return false;
    };
    let Some(changed) = git_stdout(
        &ctx.worktree,
        &[
            "diff",
            "--name-only",
            &format!("{}..HEAD", ctx.integration_base),
            "--",
            relative,
        ],
    ) else {
        return false;
    };
    changed.lines().any(|path| {
        Path::new(path).file_name().and_then(|name| name.to_str()) != Some("Cargo.toml")
    })
}

fn head_content(ctx: &PolicyContext, target: &Path) -> Result<String, String> {
    let worktree = ctx
        .worktree
        .canonicalize()
        .unwrap_or_else(|_| ctx.worktree.clone());
    let relative = target
        .strip_prefix(&worktree)
        .map_err(|_| "target is outside the canonical worktree".to_string())?;
    let relative = relative
        .to_str()
        .ok_or_else(|| "target path is not UTF-8".to_string())?;
    git_stdout(&ctx.worktree, &["show", &format!("HEAD:{relative}")])
        .ok_or_else(|| format!("cannot read HEAD:{relative}; version exceptions fail closed"))
}

fn git_stdout(worktree: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(worktree)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn lock_workspace_package_name(content: &str, changed_index: usize) -> Option<String> {
    let lines: Vec<_> = content.split_inclusive('\n').collect();
    let block_start = (0..=changed_index)
        .rev()
        .find(|index| lines[*index].trim() == "[[package]]")?;
    let block_end = lines[block_start + 1..]
        .iter()
        .position(|line| line.trim() == "[[package]]")
        .map_or(lines.len(), |offset| block_start + 1 + offset);
    if changed_index <= block_start || changed_index >= block_end {
        return None;
    }
    let names: Vec<_> = lines[block_start + 1..block_end]
        .iter()
        .filter_map(|line| parse_string_assignment(line, "name"))
        .collect();
    let has_source = lines[block_start + 1..block_end]
        .iter()
        .any(|line| parse_string_assignment(line, "source").is_some());
    (names.len() == 1 && !has_source).then(|| names[0].to_string())
}

fn parse_string_assignment<'a>(line: &'a str, wanted_key: &str) -> Option<&'a str> {
    let (key, value) = line.trim().split_once('=')?;
    if key.trim() != wanted_key {
        return None;
    }
    let value = value.trim();
    value.strip_prefix('"')?.strip_suffix('"')
}

fn check_zai_read(path: &str) -> Verdict {
    // Resolve symlinks: a benignly named link to ~/.ssh/id_rsa must be
    // judged by its target, not its name.
    let resolved = Path::new(path)
        .canonicalize()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string());
    let lower = resolved.to_lowercase();
    if ZAI_SECRET_PATHS.iter().any(|s| lower.contains(s)) {
        return Verdict::Deny(format!(
            "{path} is secret-bearing and this session runs via the Z.ai endpoint — \
             denied by provider policy"
        ));
    }
    Verdict::Allow
}

/// Follow the leaf when it is a symlink — including a DANGLING one, which
/// `exists()`/`canonicalize()` treat as absent and would otherwise be
/// reconstructed lexically inside the worktree while `Write` follows it out.
/// `None` when the chain doesn't terminate within the hop budget (or a link
/// is unreadable) — the caller fails closed.
fn resolve_leaf_symlink(path: &Path) -> Option<PathBuf> {
    let mut current = path.to_path_buf();
    for _ in 0..8 {
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let target = std::fs::read_link(&current).ok()?;
                current = if target.is_absolute() {
                    target
                } else {
                    normalize(&current.parent().unwrap_or(Path::new("/")).join(target))
                };
            }
            _ => return Some(current),
        }
    }
    None
}

/// Canonicalize the deepest existing prefix of `path`, appending the
/// not-yet-existing remainder — resolves symlinked ancestors without
/// requiring the leaf to exist.
fn canonicalize_deepest(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while !existing.exists() {
        match (existing.file_name(), existing.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name.to_os_string());
                existing = parent.to_path_buf();
            }
            _ => return path.to_path_buf(),
        }
    }
    let mut out = existing.canonicalize().unwrap_or(existing);
    for name in tail.iter().rev() {
        out.push(name);
    }
    out
}

fn check_bash(ctx: &PolicyContext, command: &str) -> Verdict {
    // Classify write TARGETS before looking for literal Cargo filenames. A
    // wildcard such as `Cargo.tom?` is expanded by the shell into the
    // controlled file, so allowing it merely because the literal substring
    // is absent would bypass the proposed-content check below. Read-only
    // commands are deliberately not subject to this path restriction.
    let write_target_text = command.lines().next().filter(|line| line.contains("<<"));
    let write_target_text = write_target_text.unwrap_or(command);
    if shell_write_target_is_unresolvable(write_target_text) {
        return Verdict::Escalate(
            "shell write target uses glob/expansion or is not a plain path; the hook cannot \
             resolve it — manifest review fails closed"
                .into(),
        );
    }

    // This is the sole shell shape whose resulting bytes are knowable without
    // executing agent text. Everything else is classified per segment below.
    match proposed_heredoc_content(command) {
        Ok((path, proposed)) if shell_token_names_controlled_cargo_file(&path) => {
            let target = match resolve_write_target(ctx, &path) {
                Ok(target) => target,
                Err(verdict) => return verdict,
            };
            if !is_controlled_cargo_file(&target) {
                return Verdict::Escalate(
                    "shell manifest edit target could not be resolved unambiguously; failing closed"
                        .into(),
                );
            }
            return check_controlled_cargo_edit(ctx, &target, &proposed);
        }
        Err(reason) if command.contains("<<") && mentions_controlled_cargo_file(command) => {
            return Verdict::Escalate(format!(
                "shell edit of a manifest: {reason}; the hook cannot determine the \
                 proposed content, so dependency/manifest review fails closed"
            ));
        }
        _ => {}
    }
    // Rules apply per pipeline segment so flags in one command cannot be
    // misread as belonging to another (`git push … && cargo test -F x`).
    for segment in shell_segments(command) {
        if let Some(reason) = controlled_cargo_shell_escalation(segment) {
            return Verdict::Escalate(format!(
                "shell edit of a manifest: {reason}; the hook cannot determine the \
                 proposed content, so dependency/manifest review fails closed"
            ));
        }
        if let v @ (Verdict::Deny(_) | Verdict::Escalate(_)) = check_bash_segment(ctx, segment) {
            return v;
        }
    }
    Verdict::Allow
}

fn shell_segments(command: &str) -> impl Iterator<Item = &str> {
    command
        .split([';', '\n', '|'])
        .flat_map(|segment| segment.split("&&"))
        .flat_map(|segment| segment.split("||"))
}

/// Whether a shell command writes through a target the hook cannot resolve to
/// one exact path. This is intentionally about write positions, not arbitrary
/// arguments: `ls crates/*/Cargo.toml` is safe, while the same token after `>`
/// or as the destination of `cp` is not.
fn shell_write_target_is_unresolvable(command: &str) -> bool {
    if shell_redirection_targets(command)
        .into_iter()
        .any(|target| !is_plain_shell_path(target))
    {
        return true;
    }

    for segment in shell_segments(command) {
        let words: Vec<_> = segment.split_whitespace().collect();
        let Some(command_index) = words.iter().position(|word| !is_shell_assignment(word)) else {
            continue;
        };
        let command_words = &words[command_index..];
        let program = command_words
            .first()
            .and_then(|word| shell_program_name(word));
        match program {
            Some("cp" | "mv" | "install" | "ln" | "rsync") if command_words.len() >= 3 => {
                if command_words
                    .last()
                    .is_some_and(|target| !is_plain_shell_path(target))
                {
                    return true;
                }
            }
            Some("dd") => {
                if command_words.iter().skip(1).any(|word| {
                    word.strip_prefix("of=")
                        .is_some_and(|target| !is_plain_shell_path(target))
                }) {
                    return true;
                }
            }
            Some("tee") => {
                let mut after_options = false;
                for target in command_words.iter().skip(1) {
                    if *target == "--" {
                        after_options = true;
                        continue;
                    }
                    if !after_options && target.starts_with('-') {
                        continue;
                    }
                    if *target != "-" && !is_plain_shell_path(target) {
                        return true;
                    }
                }
            }
            _ => {}
        }
    }
    false
}

/// Extract output-redirection targets while respecting quoted `>` bytes.
/// File-descriptor duplication (`2>&1`) is not a path write and is skipped.
fn shell_redirection_targets(command: &str) -> Vec<&str> {
    let bytes = command.as_bytes();
    let mut targets = Vec::new();
    let mut index = 0;
    let mut quote = None;
    while index < bytes.len() {
        match (quote, bytes[index]) {
            (Some(mark), byte) if byte == mark => {
                quote = None;
                index += 1;
            }
            (Some(b'"'), b'\\') | (None, b'\\') => {
                index = (index + 2).min(bytes.len());
            }
            (Some(_), _) => index += 1,
            (None, b'\'' | b'"') => {
                quote = Some(bytes[index]);
                index += 1;
            }
            (None, b'>') => {
                index += 1;
                if index < bytes.len() && matches!(bytes[index], b'>' | b'|') {
                    index += 1;
                }
                while index < bytes.len() && bytes[index].is_ascii_whitespace() {
                    index += 1;
                }
                if index < bytes.len()
                    && bytes[index] == b'&'
                    && bytes
                        .get(index + 1)
                        .is_some_and(|byte| byte.is_ascii_digit() || *byte == b'-')
                {
                    while index < bytes.len() && !bytes[index].is_ascii_whitespace() {
                        index += 1;
                    }
                    continue;
                }
                let start = index;
                let mut target_quote = None;
                while index < bytes.len() {
                    match (target_quote, bytes[index]) {
                        (Some(mark), byte) if byte == mark => {
                            target_quote = None;
                            index += 1;
                        }
                        (Some(b'"'), b'\\') | (None, b'\\') => {
                            index = (index + 2).min(bytes.len());
                        }
                        (Some(_), _) => index += 1,
                        (None, b'\'' | b'"') => {
                            target_quote = Some(bytes[index]);
                            index += 1;
                        }
                        (None, byte)
                            if byte.is_ascii_whitespace()
                                || matches!(
                                    byte,
                                    b';' | b'&' | b'|' | b'<' | b'>' | b'(' | b')'
                                ) =>
                        {
                            break;
                        }
                        _ => index += 1,
                    }
                }
                targets.push(&command[start..index]);
            }
            _ => index += 1,
        }
    }
    targets
}

fn is_plain_shell_path(token: &str) -> bool {
    if token.is_empty() || token.contains(['*', '?', '[', ']', '{', '}', '$', '`', '\\', '~']) {
        return false;
    }
    let path = if token.len() >= 2
        && ((token.starts_with('\'') && token.ends_with('\''))
            || (token.starts_with('"') && token.ends_with('"')))
    {
        &token[1..token.len() - 1]
    } else {
        token
    };
    !path.is_empty()
        && !path.starts_with('-')
        && !path
            .chars()
            .any(|ch| ch.is_whitespace() || "\"';&|<>()".contains(ch))
        && Path::new(path).components().next().is_some()
}

/// Cargo mutation commands do not expose their resulting manifest bytes or an
/// unambiguous target crate, so they are caught first. A segment which names a
/// controlled Cargo file is safe only when it is read-only or an exact
/// heredoc handled above; unrecognised editors and write forms fail closed.
fn controlled_cargo_shell_escalation(segment: &str) -> Option<String> {
    let words: Vec<_> = segment.split_whitespace().collect();
    let command_words = words
        .iter()
        .position(|word| !is_shell_assignment(word))
        .map_or(&[][..], |index| &words[index..]);
    let program = command_words
        .first()
        .and_then(|word| shell_program_name(word));
    if program == Some("cargo")
        && command_words.iter().skip(1).any(|word| {
            matches!(
                word.trim_matches(['\'', '"', '(', ')']),
                "add" | "remove" | "set-version" | "update" | "generate-lockfile" | "upgrade"
            )
        })
    {
        return Some("Cargo command may rewrite a manifest or Cargo.lock".into());
    }

    if !mentions_controlled_cargo_file(segment) {
        return None;
    }
    if is_recognised_read_only_cargo_segment(segment, command_words, program) {
        return None;
    }
    Some("command mentions Cargo.toml or Cargo.lock but is not a recognised read-only shape".into())
}

fn shell_program_name(token: &str) -> Option<&str> {
    Path::new(token.trim_matches(['\'', '"', '(', ')']))
        .file_name()
        .and_then(|name| name.to_str())
}

fn is_shell_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            && !name.as_bytes()[0].is_ascii_digit()
    })
}

fn cargo_subcommand<'a>(words: &'a [&str]) -> Option<&'a str> {
    words
        .iter()
        .skip(1)
        .find(|word| !word.starts_with('-') && !word.starts_with('+'))
        .map(|word| word.trim_matches(['\'', '"', '(', ')']))
}

fn mentions_controlled_cargo_file(segment: &str) -> bool {
    let lower = segment.to_ascii_lowercase();
    lower.contains("cargo.toml") || lower.contains("cargo.lock")
}

fn is_recognised_read_only_cargo_segment(
    segment: &str,
    words: &[&str],
    program: Option<&str>,
) -> bool {
    if controlled_cargo_is_output_target(segment) || controlled_cargo_is_output_option(words) {
        return false;
    }
    match program {
        Some("cat" | "ls" | "head" | "tail" | "wc" | "grep" | "rg" | "diff" | "stat") => true,
        Some("git") => words
            .get(1)
            .is_some_and(|subcommand| matches!(*subcommand, "diff" | "show" | "log" | "blame")),
        Some("cargo") => cargo_subcommand(words).is_some_and(|subcommand| {
            matches!(
                subcommand,
                "build" | "check" | "test" | "clippy" | "fmt" | "metadata" | "tree" | "doc"
            ) && cargo_references_only_manifest_paths(words)
        }),
        _ => false,
    }
}

fn controlled_cargo_is_output_option(words: &[&str]) -> bool {
    words.iter().enumerate().any(|(index, word)| {
        word.strip_prefix("--output=")
            .is_some_and(shell_token_names_controlled_cargo_file)
            || (*word == "--output"
                && words
                    .get(index + 1)
                    .is_some_and(|path| shell_token_names_controlled_cargo_file(path)))
    })
}

fn controlled_cargo_is_output_target(segment: &str) -> bool {
    segment.match_indices('>').any(|(at, _)| {
        let rhs = segment[at + 1..]
            .trim_start_matches('>')
            .trim_start()
            .trim_start_matches(['|', '&'])
            .trim_start();
        rhs.split_whitespace()
            .next()
            .is_some_and(shell_token_names_controlled_cargo_file)
    })
}

fn cargo_references_only_manifest_paths(words: &[&str]) -> bool {
    words.iter().enumerate().all(|(index, word)| {
        if !mentions_controlled_cargo_file(word) {
            return true;
        }
        let bare =
            word.trim_matches(|ch: char| matches!(ch, '\'' | '"' | ';' | '|' | '&' | '(' | ')'));
        if let Some(path) = bare.strip_prefix("--manifest-path=") {
            return shell_token_names_manifest(path);
        }
        index > 0 && words[index - 1] == "--manifest-path" && shell_token_names_manifest(bare)
    })
}

fn shell_token_names_manifest(token: &str) -> bool {
    Path::new(token)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("Cargo.toml"))
}

fn shell_token_names_controlled_cargo_file(token: &str) -> bool {
    let token =
        token.trim_matches(|ch: char| matches!(ch, '\'' | '"' | ';' | '|' | '&' | '(' | ')'));
    Path::new(token)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("Cargo.toml") || name == "Cargo.lock")
}

/// Accept only a single, quoted-delimiter `cat` heredoc whose bytes are the
/// exact file result. Shell substitutions, pipelines, chained commands and
/// editing programs stay denied because predicting them would mean executing
/// agent text inside the policy hook.
fn proposed_heredoc_content(command: &str) -> Result<(String, String), String> {
    let (opener, rest) = command
        .split_once('\n')
        .ok_or_else(|| "manifest-writing shell command is not a heredoc".to_string())?;
    let words: Vec<_> = opener.split_whitespace().collect();
    if words.len() != 4 {
        return Err("only `cat <<'DELIMITER' > TARGET` exposes an exact whole-file result".into());
    }
    if words[0] != "cat" {
        return Err("only a literal `cat` heredoc has an exact proposed result".into());
    }
    if words[1].starts_with("<<-") {
        return Err("tab-stripping heredocs are not reconstructed".into());
    }
    let delimiter_token = words[1]
        .strip_prefix("<<")
        .ok_or_else(|| "manifest-writing shell command is not a heredoc".to_string())?;
    let delimiter = quoted_shell_word(delimiter_token)
        .ok_or_else(|| "heredoc delimiter must be quoted so expansion is disabled".to_string())?;
    if words[2] != ">" {
        return Err("manifest heredoc must use one truncating output redirection".into());
    }
    let target = unquote_shell_word(words[3])
        .ok_or_else(|| "heredoc output target uses unsupported shell quoting".to_string())?;

    let mut proposed = String::new();
    let mut closed = false;
    let mut trailing = String::new();
    for line in rest.split_inclusive('\n') {
        if !closed && line.trim_end_matches(['\r', '\n']) == delimiter {
            closed = true;
        } else if closed {
            trailing.push_str(line);
        } else {
            proposed.push_str(line);
        }
    }
    if !closed {
        return Err("heredoc closing delimiter is missing".into());
    }
    if !trailing.trim().is_empty() {
        return Err("commands follow the manifest heredoc".into());
    }
    Ok((target, proposed))
}

fn quoted_shell_word(word: &str) -> Option<String> {
    if word.len() < 2 {
        return None;
    }
    let quoted = (word.starts_with('\'') && word.ends_with('\''))
        || (word.starts_with('"') && word.ends_with('"'));
    quoted.then(|| word[1..word.len() - 1].to_string())
}

fn unquote_shell_word(word: &str) -> Option<String> {
    if (word.starts_with('\'') && word.ends_with('\''))
        || (word.starts_with('"') && word.ends_with('"'))
    {
        return Some(word[1..word.len() - 1].to_string());
    }
    (!word.contains(['\'', '"', '`', '$', '\\'])).then(|| word.to_string())
}

fn check_bash_segment(ctx: &PolicyContext, segment: &str) -> Verdict {
    let lower = segment.to_lowercase();
    let words: Vec<&str> = lower.split_whitespace().collect();
    let original_words: Vec<&str> = segment.split_whitespace().collect();
    // git anywhere before push covers `git -C repo push` and friends.
    // Over-matching (an echo mentioning both words) errs toward deny, which
    // is the right side for a rail.
    let git_pos = words
        .iter()
        .position(|w| *w == "git" || w.ends_with("/git"));
    let is_git_push = git_pos.is_some_and(|g| words[g..].contains(&"push"));

    if let Some(git_pos) = git_pos {
        for operation in ["pull", "fetch"] {
            if let Some(operation_pos) = words[git_pos..]
                .iter()
                .position(|word| *word == operation)
                .map(|pos| git_pos + pos)
            {
                let args = &words[operation_pos + 1..];
                if args
                    .iter()
                    .any(|word| unquote_shell_word(word).as_deref() == Some("origin"))
                    && args
                        .iter()
                        .any(|word| names_task_ref(word, &ctx.task_ref_template))
                {
                    return Verdict::Deny(format!(
                        "git {operation} from origin task refs matching {:?} is denied — integrate from {} in the shared fleet repo",
                        ctx.task_ref_template, ctx.integration_branch
                    ));
                }
            }
        }
        for operation in ["reset", "rebase"] {
            if let Some(operation_pos) = words[git_pos..]
                .iter()
                .position(|word| *word == operation)
                .map(|pos| git_pos + pos)
                && words[operation_pos + 1..]
                    .iter()
                    .any(|word| names_origin_task_ref(word, &ctx.task_ref_template))
            {
                return Verdict::Deny(format!(
                    "git {operation} onto origin task refs matching {:?} is denied — integrate from {} in the shared fleet repo",
                    ctx.task_ref_template, ctx.integration_branch
                ));
            }
        }
    }

    if is_git_push {
        // Force-push: denied everywhere — long flags (incl.
        // --force-with-lease=<ref>), short -f clusters, +refspecs.
        let forced = words.iter().any(|w| {
            w.starts_with("--force")
                || (w.starts_with('-') && !w.starts_with("--") && w.contains('f'))
                || w.starts_with('+')
                || w.contains(":+")
        });
        if forced {
            return Verdict::Deny("force-push is denied everywhere".into());
        }
        // Config injection BEFORE push: `git -c remote.origin.mirror=true
        // push …` turns a plain push into a mirror. Any git-level config
        // flag on a push invocation is denied.
        let push_word = words.iter().position(|w| *w == "push").expect("checked");
        if words[..push_word]
            .iter()
            .any(|w| *w == "-c" || w.starts_with("--config") || w.starts_with("--exec-path"))
        {
            return Verdict::Deny(
                "git config-injection flags (-c/--config*/--exec-path) are denied on push".into(),
            );
        }
        // Push flags are ALLOWLISTED, not denylisted — enumerating hazardous
        // modes (--mirror, --all, --branches, --tags, --delete, --prune,
        // --follow-tags, and every future one plus every unambiguous prefix
        // git accepts) is a losing game; an unknown flag fails closed.
        let push_pos = words.iter().position(|w| *w == "push").expect("checked");
        const SAFE_PUSH_FLAGS: &[&str] = &[
            "--set-upstream",
            "--quiet",
            "--verbose",
            "--dry-run",
            "--porcelain",
            "--progress",
            "--no-progress",
            "--atomic",
            "-u",
            "-q",
            "-v",
            "-n",
        ];
        for w in &words[push_pos + 1..] {
            if w.starts_with('-')
                && !w.starts_with("--repo")
                && !SAFE_PUSH_FLAGS.contains(&w.split('=').next().unwrap_or(w))
            {
                return Verdict::Deny(format!(
                    "git push flag {w:?} is not on the safe list; push only this \
                     task's branch with plain flags"
                ));
            }
        }
        let repo_flag = original_words[push_pos + 1..]
            .iter()
            .find_map(|word| word.strip_prefix("--repo="));
        let mut positional = original_words[push_pos + 1..]
            .iter()
            .copied()
            .filter(|word| !word.starts_with('-'));
        let remote = match repo_flag {
            Some(remote) => remote,
            None => match positional.next() {
                Some(remote) => remote,
                None => {
                    return Verdict::Deny(
                        "git push has no explicit remote — push only to the shared fleet repo"
                            .into(),
                    );
                }
            },
        };
        if unquote_shell_word(remote).as_deref() == Some("origin") {
            return Verdict::Deny(
                "git push to origin is denied — task branches never go to the canonical/public remote"
                    .into(),
            );
        }
        if !push_remote_is_shared_repo(ctx, remote) {
            return Verdict::Deny(format!(
                "git push remote {remote:?} is not the shared fleet repo — task branches never go to canonical/public remotes"
            ));
        }
        match &ctx.branch {
            Some(branch) => {
                let branch_l = branch.to_lowercase();
                // Every refspec after `push` must land ON the task branch:
                // the first non-flag arg is the remote; each further
                // non-flag arg is a refspec whose destination (dst of
                // src:dst, or the ref itself) must be the branch —
                // `push origin task/7 main` and `push origin task/7:main`
                // both deny.
                let push_pos = words.iter().position(|w| *w == "push").expect("checked");
                let mut refspecs = words[push_pos + 1..].iter().filter(|w| !w.starts_with('-'));
                // `--repo=<remote>` supplies the remote as a flag — then
                // EVERY non-flag arg is a refspec, not just the tail.
                if !words.iter().any(|w| w.starts_with("--repo")) {
                    let _remote = refspecs.next();
                }
                let mut any_refspec = false;
                for spec in refspecs {
                    any_refspec = true;
                    let dst = spec.split_once(':').map(|(_, d)| d).unwrap_or(spec);
                    if dst.is_empty() || dst != branch_l.as_str() {
                        return Verdict::Deny(format!(
                            "git push refspec {spec:?} does not land on this task's \
                             branch {branch:?}"
                        ));
                    }
                }
                // Bare `git push` (no refspec) pushes the current branch to
                // its upstream — only allowed when the segment at least
                // names the task branch context; deny the ambiguous form.
                if !any_refspec && !segment.contains(branch.as_str()) {
                    return Verdict::Deny(format!(
                        "bare git push is ambiguous here; push explicitly to this \
                         task's branch {branch:?}"
                    ));
                }
            }
            None => {
                return Verdict::Deny(
                    "this task has no branch; git push is denied — record a branch via \
                     task_complete instead"
                        .into(),
                );
            }
        }
    }
    // Recursive delete: escalation class. A bare word `rm` ANYWHERE in the
    // segment followed by a recursive flag escalates — wrapper stacks
    // (`sudo -u root rm`, `env -u FOO BAR=1 rm`) cannot launder the command
    // word. Quoted occurrences (`rg 'rm -rf' docs`) keep their quote chars
    // through the whitespace split and so do not match; a rail errs toward
    // the deny side on the rest.
    for (i, w) in words.iter().enumerate() {
        if (*w == "rm" || w.ends_with("/rm"))
            && words[i + 1..].iter().any(|f| {
                *f == "--recursive"
                    || (f.starts_with('-') && !f.starts_with("--") && f.contains('r'))
            })
        {
            return Verdict::Escalate(format!("recursive delete in: {segment}"));
        }
    }
    // Other idiomatic recursive deletes / manifest edits through the shell.
    if (words.contains(&"find") && words.contains(&"-delete"))
        || (words.contains(&"git") && words.contains(&"clean"))
    {
        return Verdict::Escalate(format!("bulk delete in: {segment}"));
    }
    // Controlled Cargo-file writes were handled once, from the whole
    // command, by `check_bash`. Re-checking on the mere coexistence of a
    // manifest path and `>` misclassifies read-only output redirection.
    // Editing gates through the shell (sed/tee/redirects into gate paths).
    if is_gate_path(&gate_view(segment, &ctx.worktree)) {
        return Verdict::Deny(
            "command touches foreman's own gates — agents never modify their gates".into(),
        );
    }
    if ctx.provider == "zai" {
        let hit = ZAI_SECRET_PATHS.iter().find(|s| lower.contains(**s));
        if let Some(s) = hit {
            return Verdict::Deny(format!(
                "command references {s:?} and this session runs via the Z.ai endpoint"
            ));
        }
    }
    Verdict::Allow
}

fn names_task_ref(word: &str, template: &str) -> bool {
    let Some(word) = unquote_shell_word(word) else {
        return true;
    };
    word.trim_start_matches('+')
        .split(':')
        .any(|part| task_ref_part(part, template).is_some())
}

fn names_origin_task_ref(word: &str, template: &str) -> bool {
    let Some(word) = unquote_shell_word(word) else {
        return true;
    };
    word.trim_start_matches('+').split(':').any(|part| {
        part.strip_prefix("origin/")
            .or_else(|| part.strip_prefix("refs/remotes/origin/"))
            .is_some_and(|branch| branch_matches_template(branch, template))
    })
}

fn task_ref_part<'a>(part: &'a str, template: &str) -> Option<&'a str> {
    let branch = part
        .strip_prefix("refs/heads/")
        .or_else(|| part.strip_prefix("origin/"))
        .or_else(|| part.strip_prefix("refs/remotes/origin/"))
        .unwrap_or(part);
    branch_matches_template(branch, template).then_some(branch)
}

fn branch_matches_template(branch: &str, template: &str) -> bool {
    let Some((prefix, suffix)) = template.split_once("{id}") else {
        return false;
    };
    branch
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_suffix(suffix))
        .is_some_and(|id| !id.is_empty())
}

/// A permitted push remote must resolve to the main worktree of this task
/// worktree's own Git common directory. A sibling task worktree shares the
/// object store too, but is not the fleet integration workdir and is refused.
fn push_remote_is_shared_repo(ctx: &PolicyContext, remote: &str) -> bool {
    let Some(remote) = unquote_shell_word(remote) else {
        return false;
    };
    let configured = git_stdout(&ctx.worktree, &["remote", "get-url", "--push", &remote]);
    let target = configured.as_deref().map(str::trim).unwrap_or(&remote);
    let target = match target.strip_prefix("file://") {
        Some(path) => path,
        None if target.contains("://") || target.contains(':') => return false,
        None => target,
    };
    let target = Path::new(target);
    let target = if target.is_absolute() {
        target.to_path_buf()
    } else {
        ctx.worktree.join(target)
    };
    let Ok(target) = target.canonicalize() else {
        return false;
    };
    let Some(common) = git_stdout(
        &ctx.worktree,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ) else {
        return false;
    };
    let Ok(common) = Path::new(common.trim()).canonicalize() else {
        return false;
    };
    if target == common {
        return true;
    }
    common.file_name().is_some_and(|name| name == ".git")
        && common
            .parent()
            .and_then(|parent| parent.canonicalize().ok())
            .is_some_and(|shared_workdir| target == shared_workdir)
}

/// Lexical `..`/`.` normalization — no filesystem access, so it works for
/// paths that do not exist yet.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// The `foreman policy-check` entrypoint: PreToolUse JSON on stdin, verdict
/// as exit code (0 allow, 2 deny — the documented hook contract). Escalate
/// verdicts also file an informational finding so the handled refusal remains
/// auditable without claiming that it stopped the task.
pub fn run_check(ctx: &PolicyContext, ledger: &crate::ledger::Ledger) -> i32 {
    let mut input = String::new();
    if std::io::Read::read_to_string(&mut std::io::stdin(), &mut input).is_err() {
        eprintln!("foreman policy: could not read hook payload; denying");
        return 2;
    }
    let payload: Value = match serde_json::from_str(&input) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("foreman policy: bad hook payload ({e}); denying");
            return 2;
        }
    };
    let mut effective = ctx.clone();
    if effective.task_crates.is_empty()
        && let Ok(Some(task)) = ledger.task(ctx.task_id)
    {
        effective.task_crates = task.crates;
    }
    match evaluate(&effective, &payload) {
        Verdict::Allow => 0,
        Verdict::Deny(reason) => {
            eprintln!("policy denied: {reason}");
            record(ctx, ledger, "policy deny", &reason, "minor");
            2
        }
        Verdict::Escalate(reason) => {
            eprintln!("policy denied (escalated to operator): {reason}");
            record(ctx, ledger, "policy escalation", &reason, "info");
            2
        }
    }
}

/// Audit trail write: deduplicated (an agent retrying a denied call must
/// not flood the ledger) and loud on failure — the deny still stands either
/// way, but a lost escalation record is worth a stderr line.
fn record(
    ctx: &PolicyContext,
    ledger: &crate::ledger::Ledger,
    title: &str,
    reason: &str,
    severity: &str,
) {
    let write = || -> anyhow::Result<()> {
        let duplicate = ledger
            .open_findings(50)?
            .into_iter()
            .any(|(_, task, _, t, body)| task == Some(ctx.task_id) && t == title && body == reason);
        if !duplicate {
            ledger.file_finding(Some(ctx.task_id), severity, title, reason, "policy-gate")?;
        }
        Ok(())
    };
    if let Err(e) = write() {
        eprintln!("foreman policy: could not record the finding: {e:#}");
    }
}

/// The Claude Code settings JSON the runner passes via `--settings`: every
/// PreToolUse call runs the hook with this session's context baked in.
pub fn hook_settings(
    ctx: &PolicyContext,
    db: &Path,
    db_create: crate::state::DbCreateMode,
    project: Option<&Path>,
    foreman_bin: &Path,
) -> Value {
    let selector = match project {
        Some(path) => format!(
            "--project {} --db {} --db-create {}",
            shell_word(&path.to_string_lossy()),
            shell_word(&db.to_string_lossy()),
            db_create.as_cli_value()
        ),
        None => format!(
            "--db {} --db-create {}",
            shell_word(&db.to_string_lossy()),
            db_create.as_cli_value()
        ),
    };
    let mut cmd = format!(
        "{} {} policy-check --task {} --worktree {} --provider {} --integration-base {}",
        shell_word(&foreman_bin.to_string_lossy()),
        selector,
        ctx.task_id,
        shell_word(&ctx.worktree.to_string_lossy()),
        shell_word(&ctx.provider),
        shell_word(&ctx.integration_base),
    );
    if let Some(branch) = &ctx.branch {
        cmd.push_str(&format!(" --branch {}", shell_word(branch)));
    }
    serde_json::json!({
        "hooks": {
            "PreToolUse": [
                {
                    "matcher": "*",
                    "hooks": [ { "type": "command", "command": cmd } ]
                }
            ]
        }
    })
}

/// Resolve the caller-configured integration ref once, before installing the
/// hook. The hook receives this immutable merge-base commit rather than a
/// movable branch name, so every tool call in one session sees the same task
/// scope.
pub fn resolve_integration_base(worktree: &Path, integration_ref: &str) -> Result<String, String> {
    if integration_ref.is_empty() || integration_ref.starts_with('-') {
        return Err("integration ref is empty or option-shaped".into());
    }
    let base = git_stdout(worktree, &["merge-base", "HEAD", integration_ref])
        .ok_or_else(|| format!("cannot resolve integration ref {integration_ref:?}"))?;
    let base = base.trim();
    if base.is_empty() {
        return Err(format!(
            "integration ref {integration_ref:?} produced no merge base"
        ));
    }
    Ok(base.to_string())
}

/// Single-quote shell quoting (the hook command line is run by a shell).
fn shell_word(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}
