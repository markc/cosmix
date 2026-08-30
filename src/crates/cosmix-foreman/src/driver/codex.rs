//! Driver for `codex exec --json` — a JSONL event stream. Codex's exit codes
//! are undocumented, so `turn.completed` / `turn.failed` events are ground
//! truth and the exit status is ignored except as a tiebreaker when the
//! stream ends without either.

use std::process::{Command, ExitStatus};

use anyhow::Result;
use serde_json::Value;

use crate::executor::{
    AgentEvent, AgentKind, Budget, Executor, ExecutorCaps, RunOutcome, Session, StopReason,
    StreamParser, Usage, Workspace,
};

pub struct CodexDriver {
    program: String,
    model: Option<String>,
    sandbox: String,
    extra_args: Vec<String>,
    env: std::collections::BTreeMap<String, String>,
    sibling_repos: Option<String>,
}

impl CodexDriver {
    pub fn new() -> Self {
        CodexDriver {
            program: "codex".into(),
            model: None,
            sandbox: "workspace-write".into(),
            extra_args: Vec::new(),
            env: Default::default(),
            sibling_repos: None,
        }
    }

    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    pub fn with_program(mut self, program: impl Into<String>) -> Self {
        self.program = program.into();
        self
    }

    pub fn with_model(mut self, model: Option<String>) -> Self {
        self.model = model;
        self
    }

    pub fn with_sandbox(mut self, sandbox: impl Into<String>) -> Self {
        self.sandbox = sandbox.into();
        self
    }

    pub fn with_extra_args(mut self, args: Vec<String>) -> Self {
        self.extra_args = args;
        self
    }

    /// Fleet-local dependency clones made readable by the optional bwrap
    /// sandbox. This is supplied from the already-resolved policy snapshot.
    pub fn with_sibling_repos(mut self, sibling_repos: Option<String>) -> Self {
        self.sibling_repos = sibling_repos;
        self
    }

    /// Codex has no native budget flags; the enforceable caps (output tokens,
    /// wall clock) are runner-side kills, and the unenforceable ones are
    /// refused outright in `start()`. `budget` is still taken so the
    /// signature matches when flags do appear.
    pub fn build_args(&self, prompt: &str, budget: &Budget) -> Vec<String> {
        self.build_args_with_session(prompt, budget, None)
    }

    /// The single argv builder both the fresh and the resumed path use.
    ///
    /// ⚠️ ORDER IS LOAD-BEARING. `codex exec resume` is a clap SUBCOMMAND of
    /// `codex exec`, and it accepts only its own small option set (`-c`,
    /// `-m`, `--json`, `--last`, `--image`, …). `--sandbox` and `--add-dir`
    /// are `exec`-level options and clap rejects them after the subcommand
    /// name with `error: unexpected argument '--sandbox' found` — probed
    /// against codex-cli 0.145.0. So every exec-level flag must be emitted
    /// BEFORE the `resume <id>` pair, never after it, and both argv shapes
    /// are built here so they cannot drift apart. `codex_resume_argv_parses`
    /// in `tests/phase2.rs` runs the real CLI against this argv, because a
    /// string-shape assertion alone cannot see a clap rejection.
    ///
    /// `session` is the sole resume authority. `None` is unconditionally
    /// fresh; drivers never retain an implicit id that a fallback could
    /// accidentally reuse.
    fn build_args_with_session(
        &self,
        prompt: &str,
        _budget: &Budget,
        session: Option<&str>,
    ) -> Vec<String> {
        // Agentic-first: exec defaults to a read-only sandbox in which an
        // implementation agent "succeeds" having written nothing; default to
        // workspace-write; read-only consumers such as merge authority set
        // the one sandbox argument explicitly through `with_sandbox`.
        let mut args = vec![
            "exec".to_string(),
            "--json".into(),
            "--sandbox".into(),
            self.sandbox.clone(),
        ];
        if let Some(model) = &self.model {
            args.push("-m".into());
            args.push(model.clone());
        }
        args.extend(self.extra_args.iter().cloned());
        if let Some(session) = session {
            args.push("resume".into());
            args.push(session.to_string());
        }
        // `--` keeps a dash-leading prompt from parsing as a flag. (A bare
        // "-" prompt still means read-stdin to codex, which foreman closes.)
        args.push("--".into());
        args.push(prompt.to_string());
        args
    }

    /// [`Self::build_args`] plus the sandbox grants a writable task workspace
    /// needs. Read-only consumers do not receive external writable grants.
    ///
    /// A git WORKTREE keeps its real metadata in the MAIN checkout's `.git`
    /// (the worktree's own `.git` is just a file pointing there), and that
    /// path is outside the workspace — so under `--sandbox workspace-write`
    /// a commit cannot even create `index.lock`. Every fleet task runs in a
    /// worktree and the branch contract requires committed work, so without
    /// this the lane bounces every task having done the work correctly.
    /// Observed on the first live codex run (task 13, 2026-08-19): fmt,
    /// clippy and 87 tests green, then "a sandbox permission mismatch has
    /// blocked the required commit".
    ///
    /// This grants no more than the claude lane already has — its policy
    /// gate permits `git commit`, which writes the same objects and refs.
    ///
    /// Separate from `start()` so the argv is testable: the binary has no
    /// test harness, and a test that only checked [`git_common_dir`] would
    /// stay green if the grant were dropped from the command entirely.
    pub fn args_for(&self, prompt: &str, budget: &Budget, ws_dir: &std::path::Path) -> Vec<String> {
        let target = crate::gc::resolve_target_dir(None).ok();
        let home = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("/root"));
        self.args_for_with_paths(prompt, budget, ws_dir, target.as_deref(), &home, None)
    }

    fn args_for_with_paths(
        &self,
        prompt: &str,
        budget: &Budget,
        ws_dir: &std::path::Path,
        target: Option<&std::path::Path>,
        home: &std::path::Path,
        session: Option<&str>,
    ) -> Vec<String> {
        let mut args = self.build_args_with_session(prompt, budget, session);
        // AFTER argv[0] ("exec"): --add-dir is an exec-level flag, and codex
        // rejects it as a global one. It is also rejected after `resume`,
        // which is why this splice index is 1 on BOTH argv shapes — see the
        // order warning on `build_args_with_session`.
        if self.sandbox == "workspace-write" {
            let mut grants = Vec::new();
            if let Some(git_dir) = git_common_dir(ws_dir) {
                grants.extend(["--add-dir".into(), git_dir]);
            }
            if let Some(target) = target {
                // Codex ignores an --add-dir source that does not exist yet.
                // The dispatch unit may point at a fresh shared cache, so
                // materialise it before the child parses this grant.
                let _ = std::fs::create_dir_all(target);
                grants.extend(["--add-dir".into(), target.to_string_lossy().into_owned()]);
                grants.extend([
                    "--add-dir".into(),
                    home.join(".cargo").to_string_lossy().into_owned(),
                ]);
            }
            args.splice(1..1, grants);
        }
        args
    }

    /// [`Self::build_args`] with `turn` as the input and `resume
    /// <session_ref>` appended to the exec-level flags, so the next turn
    /// lands in the recorded conversation instead of opening a new one.
    pub fn build_resume_args(&self, session_ref: &str, turn: &str, budget: &Budget) -> Vec<String> {
        self.build_args_with_session(turn, budget, Some(session_ref))
    }

    /// [`Self::build_resume_args`] plus the same writable-directory grants
    /// [`Self::args_for`] applies, in the same position.
    pub fn resume_args_for(
        &self,
        session_ref: &str,
        turn: &str,
        budget: &Budget,
        ws_dir: &std::path::Path,
    ) -> Vec<String> {
        let target = crate::gc::resolve_target_dir(None).ok();
        let home = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("/root"));
        self.args_for_with_paths(
            turn,
            budget,
            ws_dir,
            target.as_deref(),
            &home,
            Some(session_ref),
        )
    }
}

impl Default for CodexDriver {
    fn default() -> Self {
        CodexDriver::new()
    }
}

impl Executor for CodexDriver {
    fn kind(&self) -> AgentKind {
        AgentKind::Codex
    }

    fn capabilities(&self) -> ExecutorCaps {
        ExecutorCaps {
            resume: true,
            follow_up: false,
            mcp_drivable: true,
            // Refused per check_budget above -- no native flag, no cost
            // reporting.
            enforces_cost_cap: false,
            // No native flag either, but codex reports real per-turn usage
            // after each turn -- the runner's kill-on-overage backstop can
            // act on it, which is how this lane's cap actually bites.
            enforces_token_cap: true,
        }
    }

    /// A cap that cannot be enforced must not be silently accepted: codex
    /// has no turn flag, reports no dollar cost, and its usage arrives only
    /// after each turn is already paid for.
    fn check_budget(&self, budget: &Budget) -> Result<()> {
        if budget.max_turns.is_some() || budget.max_budget_usd.is_some() {
            anyhow::bail!(
                "codex driver cannot enforce --max-turns or --max-budget-usd \
                 (no native flag, no cost reporting); use --max-output-tokens \
                 or --max-wall-secs"
            );
        }
        Ok(())
    }

    fn start(&self, prompt: &str, ws: &Workspace, budget: &Budget) -> Result<Session> {
        self.spawn(prompt, ws, budget, None)
    }

    fn resume(
        &self,
        session_ref: &str,
        turn: &str,
        ws: &Workspace,
        budget: &Budget,
    ) -> Result<Session> {
        self.spawn(turn, ws, budget, Some(session_ref))
    }
}

impl CodexDriver {
    fn spawn(
        &self,
        prompt: &str,
        ws: &Workspace,
        budget: &Budget,
        requested_resume: Option<&str>,
    ) -> Result<Session> {
        self.check_budget(budget)?;
        let mut cmd = Command::new(&self.program);
        let args = match requested_resume {
            Some(session_ref) => self.resume_args_for(session_ref, prompt, budget, &ws.dir),
            None => self.args_for(prompt, budget, &ws.dir),
        };
        cmd.args(args).current_dir(&ws.dir);
        // Drift rail (same rationale as the claude driver): the Z.ai
        // credential and its file pointer have no business in a codex
        // session's environment.
        cmd.env_remove("ZAI_API_KEY")
            .env_remove("FOREMAN_ZAI_KEY_FILE")
            // An agent subtree must never inherit verify-lane delegation
            // or verifier recursion depth.
            .env_remove(crate::verify::LANE_HELD_ENV)
            .env_remove(crate::verify::DEPTH_ENV);
        // Subscription-only by default — see the claude driver. Codex today
        // prefers its stored ChatGPT auth over OPENAI_API_KEY, but that
        // precedence is the vendor's choice to change, and the fleet should
        // not depend on it holding.
        crate::driver::scrub_metered_keys(&mut cmd);
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        let target_dir =
            crate::target_dir::pinned_target_dir(&ws.dir, ws.verify_subdir.as_deref())?;
        crate::target_dir::pin_target_dir(&mut cmd, &target_dir);
        // AFTER every env/env_remove above: `sandbox::wrap` replays this
        // command's recorded env changes onto the bwrap process it builds,
        // so anything scrubbed here stays scrubbed inside the namespace
        // too. Opt-in (FOREMAN_SANDBOX=bwrap); a no-op by default.
        let mut cmd = crate::sandbox::apply(
            cmd,
            AgentKind::Codex,
            &ws.dir,
            self.sibling_repos.as_deref(),
            None,
        )?;
        // LAST, after both caller overlays and optional sandbox wrapping.
        // Setting it on the final process also supplies the bwrap payload,
        // while `with_env` cannot restore the shared value.
        crate::target_dir::pin_target_dir(&mut cmd, &target_dir);
        Session::spawn_with_resume(
            AgentKind::Codex,
            cmd,
            Box::new(CodexParser::default()),
            requested_resume,
        )
    }
}

/// The `.git` a commit in `dir` actually writes to, absolute.
///
/// For an ordinary checkout this is `<repo>/.git`; for a WORKTREE it is the
/// MAIN checkout's `.git`, which is where the objects, the refs and the
/// per-worktree index all live. Returns `None` when `dir` is not in a repo
/// (nothing to grant) or git cannot answer — callers then simply do not
/// widen the sandbox, which fails closed.
pub fn git_common_dir(dir: &std::path::Path) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .current_dir(dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!path.is_empty()).then_some(path)
}

#[derive(Default)]
pub struct CodexParser {
    thread_id: Option<String>,
    last_message: Option<String>,
    usage: Usage,
    completed: bool,
    failed: Option<String>,
    saw_usage: bool,
}

impl StreamParser for CodexParser {
    fn parse_line(&mut self, line: &str) -> Vec<AgentEvent> {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return vec![AgentEvent::Raw {
                line: line.to_string(),
            }];
        };
        match v.get("type").and_then(Value::as_str) {
            Some("thread.started") => {
                self.thread_id = v.get("thread_id").and_then(Value::as_str).map(String::from);
                vec![AgentEvent::Started {
                    session_ref: self.thread_id.clone(),
                }]
            }
            Some("turn.started") => {
                // A new turn reopens the run: a stream that truncates inside
                // turn 2 must not report Done off turn 1's completion, and a
                // retry after turn.failed must be allowed to succeed.
                self.completed = false;
                self.failed = None;
                vec![AgentEvent::Heartbeat]
            }
            Some("item.started") | Some("item.updated") => vec![AgentEvent::Heartbeat],
            Some("item.completed") => {
                let Some(item) = v.get("item") else {
                    return Vec::new();
                };
                // Item discriminator has appeared as both `item_type` and
                // `type` across codex releases; accept either.
                let item_type = item
                    .get("item_type")
                    .or_else(|| item.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                match item_type {
                    "agent_message" => {
                        let text = item
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        self.last_message = Some(text.clone());
                        vec![AgentEvent::Text { text }]
                    }
                    "command_execution" => {
                        let detail = item
                            .get("command")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        vec![AgentEvent::ToolUse {
                            name: "command".into(),
                            detail,
                        }]
                    }
                    "reasoning" => vec![AgentEvent::Heartbeat],
                    _ => vec![AgentEvent::ToolUse {
                        name: item_type.to_string(),
                        detail: item.to_string(),
                    }],
                }
            }
            Some("turn.completed") => {
                self.completed = true;
                // turn.completed usage is cumulative session totals, not a
                // per-turn delta (openai/codex#17539) — replace, don't sum.
                // Codex reports cached input as a subset of input_tokens.
                if let Some(u) = v.get("usage") {
                    self.saw_usage = true;
                    let input = u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
                    let cached = u.get("cached_input_tokens").and_then(Value::as_u64);

                    // codex-cli 0.145.0+ reports input_tokens as the complete
                    // total. Folding cached into it again double-counts that
                    // subset.
                    self.usage.input_tokens = input;
                    self.usage.output_tokens =
                        u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);

                    // Record the two components Codex reports directly. Both
                    // are vendor readings, not derivations, so they hold
                    // whatever `input_tokens` turns out to mean.
                    self.usage.cache_read_input_tokens = cached;
                    self.usage.cache_creation_input_tokens =
                        u.get("cache_write_input_tokens").and_then(Value::as_u64);

                    // A missing cache reading leaves the split unknown. A
                    // malformed reading larger than the total is bounded at
                    // zero rather than underflowing the usage counter.
                    self.usage.fresh_input_tokens =
                        cached.map(|cached| input.saturating_sub(cached));
                }
                vec![AgentEvent::Usage {
                    usage: self.usage.clone(),
                }]
            }
            Some("turn.failed") => {
                let msg = v
                    .get("error")
                    .map(|e| {
                        e.get("message")
                            .and_then(Value::as_str)
                            .map(String::from)
                            .unwrap_or_else(|| e.to_string())
                    })
                    .unwrap_or_else(|| "turn failed".into());
                self.failed = Some(msg);
                // Keep the failure line in the event ledger.
                vec![AgentEvent::Raw {
                    line: line.to_string(),
                }]
            }
            Some("error") => {
                let msg = v
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("codex stream error")
                    .to_string();
                self.failed = Some(msg);
                vec![AgentEvent::Raw {
                    line: line.to_string(),
                }]
            }
            _ => vec![AgentEvent::Raw {
                line: line.to_string(),
            }],
        }
    }

    fn finish(self: Box<Self>, exit: Option<ExitStatus>, interrupted: bool) -> RunOutcome {
        // Event truth outranks the kill flag: a completed turn that was then
        // killed (stall or grace) is finished, paid-for work.
        let stop = if self.failed.is_some() {
            StopReason::Error
        } else if self.completed {
            StopReason::Done
        } else if interrupted {
            StopReason::Interrupted
        } else {
            StopReason::Error
        };
        let error = match stop {
            StopReason::Error => Some(self.failed.unwrap_or_else(|| {
                format!(
                    "codex stream ended without turn.completed (exit {:?})",
                    exit.and_then(|e| e.code())
                )
            })),
            _ => None,
        };
        RunOutcome {
            stop,
            result: self.last_message,
            error,
            usage: self.usage,
            session_ref: self.thread_id,
            terminal_session_ref: None,
            usage_observed: self.saw_usage,
            output_observed: false,
            resume_failure: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn resume_not_found_classifier_accepts_only_codex_real_resume_stderr() {
        const REQUESTED: &str = "019a0000-0000-7000-8000-00000000dead";
        const REAL_CLI_STDERR: &str = "Error: thread/resume: thread/resume failed: no rollout found for thread id 019a0000-0000-7000-8000-00000000dead (code -32600)\n";
        assert!(crate::executor::exact_session_not_found(
            AgentKind::Codex,
            REQUESTED,
            0,
            REAL_CLI_STDERR.len(),
            REAL_CLI_STDERR,
        ));
        assert!(!crate::executor::exact_session_not_found(
            AgentKind::Codex,
            REQUESTED,
            0,
            "no rollout found for thread id 019a0000-0000-7000-8000-00000000dead (code -32600)"
                .len(),
            "no rollout found for thread id 019a0000-0000-7000-8000-00000000dead (code -32600)",
        ));
        assert!(!crate::executor::exact_session_not_found(
            AgentKind::Codex,
            REQUESTED,
            0,
            "Not inside a trusted directory and --skip-git-repo-check was not specified.".len(),
            "Not inside a trusted directory and --skip-git-repo-check was not specified.",
        ));
    }

    #[test]
    fn resume_not_found_classifier_rejects_probe_line_with_extra_stderr_or_suffix() {
        const REQUESTED: &str = "019a0000-0000-7000-8000-00000000dead";
        const PROBE_LINE: &str = "Error: thread/resume: thread/resume failed: no rollout found for thread id 019a0000-0000-7000-8000-00000000dead (code -32600)";
        for stderr in [
            format!("other stderr\n{PROBE_LINE}"),
            format!("{PROBE_LINE}\nother stderr"),
            format!("{PROBE_LINE} suffix"),
        ] {
            assert!(!crate::executor::exact_session_not_found(
                AgentKind::Codex,
                REQUESTED,
                0,
                stderr.len(),
                &stderr,
            ));
        }
        assert!(!crate::executor::exact_session_not_found(
            AgentKind::Codex,
            REQUESTED,
            1,
            PROBE_LINE.len(),
            PROBE_LINE,
        ));
    }

    #[test]
    fn resume_not_found_rejects_whitespace_stdout() {
        const REQUESTED: &str = "019a0000-0000-7000-8000-00000000dead";
        const PROBE_LINE: &str = "Error: thread/resume: thread/resume failed: no rollout found for thread id 019a0000-0000-7000-8000-00000000dead (code -32600)\n";
        assert!(!crate::executor::exact_session_not_found(
            AgentKind::Codex,
            REQUESTED,
            1,
            PROBE_LINE.len(),
            PROBE_LINE,
        ));
    }

    #[test]
    fn resume_not_found_rejects_probe_line_after_truncated_stderr() {
        const REQUESTED: &str = "019a0000-0000-7000-8000-00000000dead";
        const PROBE_LINE: &str = "Error: thread/resume: thread/resume failed: no rollout found for thread id 019a0000-0000-7000-8000-00000000dead (code -32600)\n";
        assert!(!crate::executor::exact_session_not_found(
            AgentKind::Codex,
            REQUESTED,
            0,
            8 * 1024 + PROBE_LINE.len(),
            PROBE_LINE,
        ));
    }

    struct EnvRestore(Vec<(&'static str, Option<std::ffi::OsString>)>);

    impl EnvRestore {
        fn save(names: &[&'static str]) -> Self {
            Self(
                names
                    .iter()
                    .map(|name| (*name, std::env::var_os(name)))
                    .collect(),
            )
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            for (name, value) in &self.0 {
                unsafe {
                    match value {
                        Some(value) => std::env::set_var(name, value),
                        None => std::env::remove_var(name),
                    }
                }
            }
        }
    }

    fn grants(args: &[String]) -> Vec<&str> {
        args.windows(2)
            .filter_map(|pair| (pair[0] == "--add-dir").then_some(pair[1].as_str()))
            .collect()
    }

    #[test]
    fn json_progress_events_become_stall_clock_heartbeats() {
        let mut parser = CodexParser::default();
        for line in [
            r#"{"type":"turn.started"}"#,
            r#"{"type":"item.started","item":{"type":"reasoning"}}"#,
            r#"{"type":"item.updated","item":{"type":"reasoning"}}"#,
            r#"{"type":"item.completed","item":{"type":"reasoning"}}"#,
        ] {
            let events = parser.parse_line(line);
            assert_eq!(events.len(), 1, "{line}");
            assert!(matches!(events[0], AgentEvent::Heartbeat), "{line}");
        }
    }

    #[test]
    fn args_for_grants_build_dirs_only_for_workspace_write_with_shared_target() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let status = Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .status()
            .unwrap();
        assert!(status.success());

        let home = tmp.path().join("home");
        let target = home.join("shared-target");
        let cargo_home = home.join(".cargo");
        let driver = CodexDriver::new();
        let args =
            driver.args_for_with_paths("p", &Budget::default(), &repo, Some(&target), &home, None);
        let git_dir = git_common_dir(&repo).unwrap();
        assert_eq!(args[0], "exec");
        assert_eq!(
            grants(&args),
            vec![
                git_dir.as_str(),
                target.to_str().unwrap(),
                cargo_home.to_str().unwrap(),
            ],
            "all writable grants must follow `exec`: {args:?}"
        );
        assert!(target.is_dir(), "the shared target must exist before exec");

        let args = driver.args_for_with_paths("p", &Budget::default(), &repo, None, &home, None);
        assert_eq!(
            grants(&args),
            vec![git_dir.as_str()],
            "without CARGO_TARGET_DIR only the git grant is needed: {args:?}"
        );

        let args = driver.with_sandbox("read-only").args_for_with_paths(
            "p",
            &Budget::default(),
            &repo,
            Some(&target),
            &home,
            None,
        );
        assert!(
            grants(&args).is_empty(),
            "non-workspace-write sandboxes must receive no grants: {args:?}"
        );
    }

    #[test]
    fn build_resume_args_uses_exec_resume_with_the_recorded_thread_and_new_turn() {
        let driver = CodexDriver::new().with_model(Some("gpt-5.6-sol".into()));
        let args = driver.build_resume_args("thread-9", "the fixes landed", &Budget::default());

        assert_eq!(args[0], "exec");
        let resume = resume_at(&args);
        assert_eq!(args[resume + 1], "thread-9");
        assert!(args.contains(&"--json".to_string()));
        let dash_dash = args.iter().position(|a| a == "--").unwrap();
        assert_eq!(args[dash_dash + 1], "the fixes landed");
    }

    /// The blocker this argv shape exists to avoid: `codex exec resume` is a
    /// clap subcommand with its OWN option set, and `--sandbox`/`--add-dir`
    /// are `exec`-level. Emitting them after `resume` makes the CLI refuse
    /// the whole invocation ("unexpected argument '--sandbox' found"), which
    /// no string-equality test catches because the argv still "looks right".
    #[test]
    fn every_exec_level_flag_precedes_the_resume_subcommand() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _restore = EnvRestore::save(&["HOME", crate::gc::TARGET_DIR_ENV]);
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .status()
            .unwrap();
        unsafe { std::env::set_var(crate::gc::TARGET_DIR_ENV, tmp.path().join("target")) };

        let driver = CodexDriver::new().with_model(Some("gpt-5.6-sol".into()));
        let args = driver.resume_args_for("thread-9", "turn", &Budget::default(), &repo);
        let resume = resume_at(&args);

        for flag in ["--json", "--sandbox", "--add-dir", "-m"] {
            let at = args
                .iter()
                .position(|a| a == flag)
                .unwrap_or_else(|| panic!("{flag} missing from resume argv: {args:?}"));
            assert!(
                at < resume,
                "{flag} is an exec-level option and must precede `resume`: {args:?}"
            );
        }
        // ... and nothing but the id and the `-- <turn>` pair after it.
        assert_eq!(&args[resume..], ["resume", "thread-9", "--", "turn"]);
    }

    #[test]
    fn fresh_and_resume_argv_are_explicit_and_disjoint() {
        let driver = CodexDriver::new();
        assert!(
            !driver
                .build_args("fresh", &Budget::default())
                .iter()
                .any(|arg| arg == "resume")
        );
        let args = driver.build_resume_args("explicit", "turn", &Budget::default());

        assert_eq!(
            args.iter().filter(|a| *a == "resume").count(),
            1,
            "exactly one resume subcommand: {args:?}"
        );
        assert_eq!(args[resume_at(&args) + 1], "explicit");
    }

    fn resume_at(args: &[String]) -> usize {
        args.iter()
            .position(|a| a == "resume")
            .unwrap_or_else(|| panic!("no `resume` subcommand in {args:?}"))
    }

    #[test]
    fn resume_args_for_grants_the_same_dirs_as_args_for() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _restore = EnvRestore::save(&["HOME", crate::gc::TARGET_DIR_ENV]);
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .status()
            .unwrap();
        unsafe {
            std::env::remove_var(crate::gc::TARGET_DIR_ENV);
        }

        let driver = CodexDriver::new();
        let fresh = driver.args_for("p", &Budget::default(), &repo);
        let resumed = driver.resume_args_for("thread-9", "p", &Budget::default(), &repo);

        assert_eq!(grants(&fresh), grants(&resumed));
        assert_eq!(resumed[0], "exec");
        let resume = resume_at(&resumed);
        assert_eq!(resumed[resume + 1], "thread-9");
    }
}
