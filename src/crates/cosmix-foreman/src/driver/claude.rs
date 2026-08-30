//! Driver for `claude -p --output-format stream-json` — and, with three env
//! vars swapped, for GLM via Z.ai's Anthropic-compatible endpoint (the GLM
//! worker *is* a Claude Code process; it inherits the whole harness).

use std::collections::{BTreeMap, BTreeSet};
use std::process::{Command, ExitStatus};

use anyhow::Result;
use serde_json::Value;

use crate::executor::{
    AgentEvent, AgentKind, Budget, Executor, ExecutorCaps, RunOutcome, Session, StopReason,
    StreamParser, Usage, Workspace,
};

pub const ZAI_ANTHROPIC_URL: &str = "https://api.z.ai/api/anthropic";
/// Z.ai's own recommendation — long GLM generations are slow.
pub const ZAI_TIMEOUT_MS: &str = "3000000";
/// The context window the CLI's auto-compactor plans against on the GLM
/// lane. The CLI sizes compaction by the model NAME it believes it is
/// running — under Z.ai's remap that is a Claude-5 name with a 1M window —
/// not by what the endpoint accepts, so it never compacted and Z.ai
/// refused the request instead: 6 of 7 GLM attempts on 2026-08-22 died
/// with "API Error: The model has reached its context window limit" at
/// ~550k input tokens, with zero compaction events in the stream.
/// `CLAUDE_CODE_AUTO_COMPACT_WINDOW` is first in the CLI's resolution
/// chain (env, settings, client data, experiment, model default). First
/// set to 200k (0.7.10); corrected to 120k with [`ZAI_MAX_OUTPUT_TOKENS`]
/// below once the transcripts showed the real wall at ~138k of context.
/// (The CLI also offers `CLAUDE_CODE_MAX_CONTEXT_TOKENS`, the model's
/// declared window; not set here, since it may govern more than compaction.)
pub const ZAI_AUTO_COMPACT_WINDOW: &str = "120000";
/// Per-response output cap on the GLM lane. The 200000 compact window
/// above was a misdiagnosis: every GLM session that died with "API Error:
/// The model has reached its context window limit" (tasks 8, 17, 19, 31,
/// 32, 36, 37, 38, 43 — twelve transcripts) did so at a peak context of
/// 137-139k tokens with the CLI-recorded error `max_output_tokens`, while
/// glm-5.3 accepts a 270k-token request directly. So the wall is the
/// remapped model's `input + max_tokens` ceiling hit by the CLI's default
/// per-response `max_tokens`, not the window — and compaction planned
/// against 200k never ran. Cap the response at 32k (GLM-5.3 allows 131k;
/// no agent turn needs more) and compact at 120k, comfortably under the
/// observed wall.
pub const ZAI_MAX_OUTPUT_TOKENS: &str = "32768";

/// Claude Code's normal background-task promise needs a later interactive
/// turn. `claude -p` has no such turn: returning the final answer tears the
/// process down. This is appended to the system prompt for both the native
/// Claude and GLM lanes, which share this driver and CLI.
pub const HEADLESS_SYSTEM_PROMPT: &str = "This is a single-turn headless session. Never use Bash run_in_background, shell backgrounding, or any background Bash command: no later turn exists to receive its completion notification. Run every gate in the foreground with an explicit timeout. Ending your final response ends the run, so commit all work before the final message.";

/// Machine-readable runner classification carried in the parser error when a
/// headless Claude Code process exits with background Bash still live.
pub const AGENT_ABANDONED_BACKGROUND: &str = "agent_abandoned_background";

pub struct ClaudeDriver {
    kind: AgentKind,
    program: String,
    model: Option<String>,
    permission_mode: Option<String>,
    env: BTreeMap<String, String>,
    extra_args: Vec<String>,
    sibling_repos: Option<String>,
    hook_mounts: Option<crate::sandbox::HookMounts>,
}

impl ClaudeDriver {
    pub fn new() -> Self {
        ClaudeDriver {
            kind: AgentKind::Claude,
            program: "claude".into(),
            model: None,
            permission_mode: None,
            env: BTreeMap::new(),
            extra_args: Vec::new(),
            sibling_repos: None,
            hook_mounts: None,
        }
    }

    /// The GLM worker: same binary, Z.ai endpoint, `provider=zai` semantics.
    /// Under tier remapping "opus" is GLM-5.3 — label sessions by [`AgentKind`],
    /// never by the tier name the CLI reports.
    pub fn glm(zai_token: &str) -> Self {
        let mut d = ClaudeDriver::new();
        d.kind = AgentKind::Glm;
        d.env
            .insert("ANTHROPIC_BASE_URL".into(), ZAI_ANTHROPIC_URL.into());
        d.env
            .insert("ANTHROPIC_AUTH_TOKEN".into(), zai_token.into());
        d.env.insert("API_TIMEOUT_MS".into(), ZAI_TIMEOUT_MS.into());
        d.env.insert(
            "CLAUDE_CODE_AUTO_COMPACT_WINDOW".into(),
            ZAI_AUTO_COMPACT_WINDOW.into(),
        );
        d.env.insert(
            "CLAUDE_CODE_MAX_OUTPUT_TOKENS".into(),
            ZAI_MAX_OUTPUT_TOKENS.into(),
        );
        d
    }

    pub fn with_program(mut self, program: impl Into<String>) -> Self {
        self.program = program.into();
        self
    }

    pub fn with_model(mut self, model: Option<String>) -> Self {
        self.model = model;
        self
    }

    pub fn with_permission_mode(mut self, mode: Option<String>) -> Self {
        self.permission_mode = mode;
        self
    }

    pub fn with_extra_args(mut self, args: Vec<String>) -> Self {
        self.extra_args = args;
        self
    }

    pub fn with_sibling_repos(mut self, repos: Option<String>) -> Self {
        self.sibling_repos = repos;
        self
    }

    pub fn with_hook_mounts(mut self, mounts: Option<crate::sandbox::HookMounts>) -> Self {
        self.hook_mounts = mounts;
        self
    }

    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// stream-json in print mode requires --verbose; budgets ride the two
    /// native flags and surface as exit code 2 (partial results).
    pub fn build_args(&self, prompt: &str, budget: &Budget) -> Vec<String> {
        self.build_args_with_session(prompt, budget, None)
    }

    /// The single argv builder both the fresh and the resumed path use.
    ///
    /// `session` is the sole resume authority. `None` is unconditionally
    /// fresh; drivers never retain an implicit id that a fallback could
    /// accidentally reuse.
    fn build_args_with_session(
        &self,
        prompt: &str,
        budget: &Budget,
        session: Option<&str>,
    ) -> Vec<String> {
        let mut args = vec![
            "-p".to_string(),
            prompt.to_string(),
            "--output-format".into(),
            "stream-json".into(),
            "--verbose".into(),
            "--append-system-prompt".into(),
            HEADLESS_SYSTEM_PROMPT.into(),
        ];
        if let Some(model) = &self.model {
            args.push("--model".into());
            args.push(model.clone());
        }
        if let Some(session) = session {
            args.push("--resume".into());
            args.push(session.to_string());
        }
        // Agentic-first (2026-08-16 law): the unattended path is the default;
        // in -p mode there is nobody to answer a prompt, and a permission-
        // starved run reports success having done nothing. Guard rails are
        // opt-in via --permission-mode.
        let mode = self
            .permission_mode
            .as_deref()
            .unwrap_or("bypassPermissions");
        args.push("--permission-mode".into());
        args.push(mode.to_string());
        if let Some(turns) = budget.max_turns {
            args.push("--max-turns".into());
            args.push(turns.to_string());
        }
        if let Some(usd) = budget.max_budget_usd {
            args.push("--max-budget-usd".into());
            args.push(usd.to_string());
        }
        args.extend(self.extra_args.iter().cloned());
        args
    }

    /// [`Self::build_args`] with `turn` as the prompt plus `--resume
    /// <session_ref>`. `claude --resume` looks the conversation up by
    /// session id AND the working directory it was started in — the caller
    /// must invoke this against the SAME worktree the original session used,
    /// or the CLI will not find it and this run reads as a fresh session
    /// under a borrowed id.
    pub fn build_resume_args(&self, session_ref: &str, turn: &str, budget: &Budget) -> Vec<String> {
        self.build_args_with_session(turn, budget, Some(session_ref))
    }

    /// The command-building and env-scrubbing common to a fresh start and a
    /// resume — only the argv differs.
    fn spawn(
        &self,
        args: Vec<String>,
        ws: &Workspace,
        budget: &Budget,
        requested_resume: Option<&str>,
    ) -> Result<Session> {
        self.check_budget(budget)?;
        let mut cmd = Command::new(&self.program);
        cmd.args(args).current_dir(&ws.dir);
        // A stray redirect in the operator's shell must not silently retarget
        // a session; GLM additionally drops ANTHROPIC_API_KEY, which would
        // out-rank the AUTH_TOKEN the Z.ai endpoint needs. The Z.ai secret
        // and its file pointer are scrubbed too — a drift rail keeping the
        // credential out of non-GLM sessions' casual reach, NOT same-UID
        // containment (that boundary is the vendor sandbox; a same-user
        // agent can read the key's on-disk homes regardless).
        // With the optional bwrap view enabled this is cross-lane filesystem
        // isolation; with the default-off view it is an inheritance rail only.
        cmd.env_remove("ANTHROPIC_BASE_URL")
            .env_remove("ANTHROPIC_AUTH_TOKEN")
            .env_remove("ZAI_API_KEY")
            .env_remove("FOREMAN_ZAI_KEY_FILE")
            // An agent subtree must never inherit verify-lane delegation
            // or verifier recursion depth.
            .env_remove(crate::verify::LANE_HELD_ENV)
            .env_remove(crate::verify::DEPTH_ENV);
        // The CLONE lane goes the other way: this driver is how the merge
        // authority's review session is spawned, and `refine` holds the
        // clone lane across that whole call. A child that re-acquired it
        // would block on its own parent until the wait expired, so it is
        // told to join. No-op when nothing holds the lane (the ordinary
        // dispatch path), so agent sessions are unaffected.
        crate::clone_lock::export_lane_marker(&mut cmd);
        // SUBSCRIPTION-ONLY BY DEFAULT. An `ANTHROPIC_API_KEY` in the
        // environment silently outranks the claude.ai OAuth login, so an
        // unattended fleet would switch to metered API billing with no
        // signal anywhere — the ledger's cost figures look identical either
        // way, because the CLI reports list price regardless. GLM must lose
        // it too: it would out-rank the AUTH_TOKEN the Z.ai endpoint needs.
        cmd.env_remove("ANTHROPIC_API_KEY");
        crate::driver::scrub_metered_keys(&mut cmd);
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        // LAST, after every lane and caller-supplied overlay: an agent's ad
        // hoc cargo is pinned to the same canonical target tier 0 verifies and
        // `with_env` cannot restore a cross-worktree-collidable target.
        let target_dir =
            crate::target_dir::pinned_target_dir(&ws.dir, ws.verify_subdir.as_deref())?;
        crate::target_dir::pin_target_dir(&mut cmd, &target_dir);
        // Compose the hook's absolute paths into the same mount view only
        // after all environment scrubbing. Opt-in via FOREMAN_SANDBOX; off
        // remains a byte-for-byte ordinary Claude/GLM launch.
        let mut cmd = crate::sandbox::apply(
            cmd,
            self.kind,
            &ws.dir,
            self.sibling_repos.as_deref(),
            self.hook_mounts.as_ref(),
        )?;
        // Keep the payload pinned after wrapping as well: with_env cannot
        // restore a shared target, and bwrap itself receives the same value.
        crate::target_dir::pin_target_dir(&mut cmd, &target_dir);
        let budgeted = budget.max_turns.is_some() || budget.max_budget_usd.is_some();
        Session::spawn_with_resume(
            self.kind,
            cmd,
            Box::new(ClaudeParser::new(budgeted)),
            requested_resume,
        )
    }
}

impl Default for ClaudeDriver {
    fn default() -> Self {
        ClaudeDriver::new()
    }
}

impl Executor for ClaudeDriver {
    fn kind(&self) -> AgentKind {
        self.kind
    }

    fn capabilities(&self) -> ExecutorCaps {
        ExecutorCaps {
            resume: true,
            follow_up: false,
            mcp_drivable: false,
            // Refused per check_budget above -- GLM traffic prices at Anthropic
            // rates over this CLI, which is fiction against Z.ai billing.
            enforces_cost_cap: self.kind == AgentKind::Claude,
            // stream-json reports real per-turn usage for both kinds
            // (only the dollar figure is Claude-only fiction for GLM); the
            // runner's kill-on-overage backstop can act on it.
            enforces_token_cap: true,
        }
    }

    /// The claude CLI prices the remapped tier at Anthropic rates — fiction
    /// against Z.ai billing. Refuse rather than cap a fantasy.
    fn check_budget(&self, budget: &Budget) -> Result<()> {
        if self.kind == AgentKind::Glm && budget.max_budget_usd.is_some() {
            anyhow::bail!(
                "glm driver cannot enforce --max-budget-usd (the claude CLI reports \
                 Anthropic-priced costs for Z.ai traffic); use --max-output-tokens \
                 or --max-wall-secs"
            );
        }
        Ok(())
    }

    fn start(&self, prompt: &str, ws: &Workspace, budget: &Budget) -> Result<Session> {
        self.spawn(self.build_args(prompt, budget), ws, budget, None)
    }

    fn resume(
        &self,
        session_ref: &str,
        turn: &str,
        ws: &Workspace,
        budget: &Budget,
    ) -> Result<Session> {
        self.spawn(
            self.build_resume_args(session_ref, turn, budget),
            ws,
            budget,
            Some(session_ref),
        )
    }
}

/// Parses the documented stream-json line protocol: `system`/`assistant`/
/// `user`/`result` objects, one per line. Unknown shapes surface as
/// [`AgentEvent::Raw`] rather than being dropped.
#[derive(Default)]
pub struct ClaudeParser {
    session_ref: Option<String>,
    terminal_session_ref: Option<String>,
    result: Option<String>,
    is_error: bool,
    usage: Usage,
    saw_result: bool,
    result_subtype: Option<String>,
    saw_usage: bool,
    /// Background Bash tasks have edge bookends in Claude Code's stream.
    /// A clean `result` is not actually clean while one remains live: in
    /// print mode the process exits and kills it instead of scheduling the
    /// interactive follow-up turn that `run_in_background` promises.
    local_bash: BTreeSet<String>,
    background_bash: BTreeSet<String>,
    killed_background_bash: BTreeSet<String>,
    /// Whether the invocation carried native budget flags — exit code 2 is
    /// the whole "blocking error" family (auth, permission, limits), so it
    /// only reads as BudgetCeiling when a budget was actually set.
    budgeted: bool,
}

impl ClaudeParser {
    pub fn new(budgeted: bool) -> Self {
        ClaudeParser {
            budgeted,
            ..Default::default()
        }
    }

    fn message_events(&mut self, msg: &Value) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        if let Some(usage) = msg.get("usage") {
            accumulate_usage(&mut self.usage, usage, self.saw_usage);
            self.saw_usage = true;
        }
        for block in msg
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        out.push(AgentEvent::Text {
                            text: text.to_string(),
                        });
                    }
                }
                Some("tool_use") => {
                    let name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_string();
                    let detail = block
                        .get("input")
                        .map(|v| v.to_string())
                        .unwrap_or_default();
                    out.push(AgentEvent::ToolUse { name, detail });
                }
                // Thinking blocks are known noise; genuinely unknown block
                // types surface as Raw rather than vanishing.
                Some("thinking") | Some("redacted_thinking") => {}
                _ => out.push(AgentEvent::Raw {
                    line: block.to_string(),
                }),
            }
        }
        out
    }

    fn track_task_event(&mut self, event: &Value) {
        let subtype = event.get("subtype").and_then(Value::as_str);
        let Some(task_id) = event.get("task_id").and_then(Value::as_str) else {
            return;
        };
        match subtype {
            Some("task_started") => {
                if event.get("task_type").and_then(Value::as_str) == Some("local_bash") {
                    self.local_bash.insert(task_id.to_string());
                    if event.get("is_backgrounded").and_then(Value::as_bool) == Some(true) {
                        self.background_bash.insert(task_id.to_string());
                    }
                }
            }
            Some("task_updated" | "task_notification") => {
                let patch = event.get("patch");
                let is_backgrounded = event
                    .get("is_backgrounded")
                    .or_else(|| patch.and_then(|patch| patch.get("is_backgrounded")))
                    .and_then(Value::as_bool);
                // Long foreground Bash calls are automatically moved to the
                // background after the Claude Code tool's own timeout. The
                // real task-44 stream starts with `is_backgrounded:false`
                // and only announces the transition in this patch.
                if is_backgrounded == Some(true) && self.local_bash.contains(task_id) {
                    self.background_bash.insert(task_id.to_string());
                }
                let status = event
                    .get("status")
                    .or_else(|| patch.and_then(|patch| patch.get("status")))
                    .and_then(Value::as_str);
                match status {
                    Some("killed") => {
                        // A killed update after the result line is Claude
                        // Code tearing down its orphan. An older CLI may omit
                        // the background-transition flag, but it must still
                        // have identified this task as local Bash: an unknown
                        // task is not guessed to be background work.
                        if self.background_bash.remove(task_id)
                            || (self.saw_result && self.local_bash.contains(task_id))
                        {
                            self.killed_background_bash.insert(task_id.to_string());
                        }
                        self.local_bash.remove(task_id);
                    }
                    Some("completed" | "failed" | "stopped") => {
                        self.background_bash.remove(task_id);
                        self.local_bash.remove(task_id);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn abandoned_background_detail(&self) -> Option<String> {
        let task_ids: BTreeSet<_> = self
            .background_bash
            .iter()
            .chain(&self.killed_background_bash)
            .cloned()
            .collect();
        (!task_ids.is_empty()).then(|| {
            format!(
                "{AGENT_ABANDONED_BACKGROUND}: single-turn `claude -p` ended while \
                 background Bash task(s) {} were still live or killed during teardown",
                task_ids.into_iter().collect::<Vec<_>>().join(", ")
            )
        })
    }
}

fn accumulate_usage(usage: &mut Usage, v: &Value, has_prior: bool) {
    // Cache reads/writes are billed tokens the plain input count omits;
    // fold them in so token caps track real volume.
    let fresh = v.get("input_tokens").and_then(Value::as_u64);
    let cache_read = v.get("cache_read_input_tokens").and_then(Value::as_u64);
    let cache_creation = v.get("cache_creation_input_tokens").and_then(Value::as_u64);

    // Folded total for cap enforcement (preserves existing behaviour)
    usage.input_tokens +=
        fresh.unwrap_or(0) + cache_read.unwrap_or(0) + cache_creation.unwrap_or(0);
    usage.output_tokens += v.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);

    // A component total is knowable only when every contributing usage block
    // reported it. Explicit zero remains Some(0); an omitted field is None.
    accumulate_component(&mut usage.fresh_input_tokens, fresh, has_prior);
    accumulate_component(&mut usage.cache_read_input_tokens, cache_read, has_prior);
    accumulate_component(
        &mut usage.cache_creation_input_tokens,
        cache_creation,
        has_prior,
    );
}

fn accumulate_component(total: &mut Option<u64>, value: Option<u64>, has_prior: bool) {
    *total = if has_prior {
        total.and_then(|current| value.map(|value| current + value))
    } else {
        value
    };
}

/// Flatten a `tool_result` content value (string or block array) into a
/// bounded detail string for the event ledger.
fn tool_result_detail(content: &Value) -> String {
    bounded_detail(match content {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    })
}

/// Bound one ledger detail string, on a char boundary.
pub(crate) fn bounded_detail(s: String) -> String {
    const CAP: usize = 4 * 1024;
    let mut s = s;
    if s.len() > CAP {
        let cut = s
            .char_indices()
            .map(|(i, _)| i)
            .take_while(|&i| i <= CAP)
            .last()
            .unwrap_or(0);
        s.truncate(cut);
        s.push('…');
    }
    s
}

impl StreamParser for ClaudeParser {
    fn parse_line(&mut self, line: &str) -> Vec<AgentEvent> {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return vec![AgentEvent::Raw {
                line: line.to_string(),
            }];
        };
        match v.get("type").and_then(Value::as_str) {
            Some("system") => {
                let subtype = v.get("subtype").and_then(Value::as_str);
                if subtype == Some("init") {
                    self.session_ref = v
                        .get("session_id")
                        .and_then(Value::as_str)
                        .map(String::from);
                    return vec![AgentEvent::Started {
                        session_ref: self.session_ref.clone(),
                    }];
                }
                if matches!(
                    subtype,
                    Some(
                        "task_started"
                            | "task_updated"
                            | "task_notification"
                            | "background_tasks_changed"
                    )
                ) {
                    if subtype == Some("background_tasks_changed") {
                        // This snapshot independently identifies background
                        // local Bash and covers CLI versions that omit the
                        // separate is_backgrounded patch. Do not clear on an
                        // empty teardown snapshot: the following killed event
                        // is the evidence we need to preserve.
                        for task in v
                            .get("tasks")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                        {
                            if task.get("task_type").and_then(Value::as_str) == Some("local_bash")
                                && let Some(task_id) = task.get("task_id").and_then(Value::as_str)
                            {
                                self.local_bash.insert(task_id.to_string());
                                self.background_bash.insert(task_id.to_string());
                            }
                        }
                    } else {
                        self.track_task_event(&v);
                    }
                }
                vec![AgentEvent::Raw {
                    line: line.to_string(),
                }]
            }
            Some("assistant") => match v.get("message") {
                Some(msg) => {
                    let mut evs = self.message_events(msg);
                    evs.push(AgentEvent::Usage {
                        usage: self.usage.clone(),
                    });
                    evs
                }
                None => vec![AgentEvent::Raw {
                    line: line.to_string(),
                }],
            },
            Some("user") => {
                // Tool results (including failed ones) belong in the event
                // ledger; anything else user-typed still proves liveness.
                let mut evs: Vec<AgentEvent> = v
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
                    .map(|b| AgentEvent::ToolResult {
                        detail: b.get("content").map(tool_result_detail).unwrap_or_default(),
                    })
                    .collect();
                if evs.is_empty() {
                    evs.push(AgentEvent::Heartbeat);
                }
                evs
            }
            Some("result") => {
                self.saw_result = true;
                self.result_subtype = v.get("subtype").and_then(Value::as_str).map(String::from);
                self.is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                self.result = v.get("result").and_then(Value::as_str).map(String::from);
                self.terminal_session_ref = v
                    .get("session_id")
                    .and_then(Value::as_str)
                    .map(String::from);
                // Some stream-json versions omit the id from `system/init`
                // but include it in the terminal result. Preserve that
                // established conversation for the next sweep. When init
                // did carry an id, keep both values distinct so the executor
                // can still reject an init/result identity disagreement.
                if self.session_ref.is_none() {
                    self.session_ref = self.terminal_session_ref.clone();
                }
                // The result line's usage is authoritative for the whole
                // session; replace the running per-message accumulation.
                if let Some(usage) = v.get("usage") {
                    let mut total = Usage::default();
                    accumulate_usage(&mut total, usage, false);
                    total.cost_usd = self.usage.cost_usd;
                    self.usage = total;
                    self.saw_usage = true;
                }
                if let Some(cost) = v.get("total_cost_usd").and_then(Value::as_f64) {
                    self.usage.cost_usd = Some(cost);
                }
                vec![AgentEvent::Usage {
                    usage: self.usage.clone(),
                }]
            }
            _ => vec![AgentEvent::Raw {
                line: line.to_string(),
            }],
        }
    }

    fn finish(self: Box<Self>, exit: Option<ExitStatus>, interrupted: bool) -> RunOutcome {
        let code = exit.and_then(|e| e.code());
        // A runner-initiated interruption (budget/stall/operator kill) owns
        // the cause. Only a naturally ending print-mode process proves the
        // agent returned while its background Bash could not report back.
        if !interrupted
            && code == Some(0)
            && self.saw_result
            && !self.is_error
            && let Some(error) = self.abandoned_background_detail()
        {
            return RunOutcome {
                stop: StopReason::Error,
                result: self.result,
                error: Some(error),
                usage: self.usage,
                session_ref: self.session_ref,
                terminal_session_ref: self.terminal_session_ref,
                usage_observed: self.saw_usage,
                output_observed: false,
                resume_failure: None,
            };
        }
        let clean = self.saw_result && !self.is_error;
        // "error_max_*" subtypes (max turns / max budget) are the CLI saying
        // the ceiling itself was hit — exit code 2 alone is the whole
        // blocking-error family (auth, permission, limits) and proves nothing
        // about budgets, even on a budgeted run.
        let budget_hit = self.budgeted
            && self
                .result_subtype
                .as_deref()
                .is_some_and(|s| s.starts_with("error_max"));
        let stop = if clean && (code == Some(0) || interrupted) {
            // A kill that lands after a clean result already streamed is a
            // completed run — retrying it would duplicate paid-for work.
            StopReason::Done
        } else if budget_hit {
            StopReason::BudgetCeiling
        } else if self.saw_result && self.is_error {
            // The agent's own error result is the diagnosis; it outranks the
            // interrupted flag, which would discard it.
            StopReason::Error
        } else if interrupted {
            StopReason::Interrupted
        } else {
            StopReason::Error
        };
        let error = match stop {
            StopReason::Error => Some(if self.is_error {
                self.result
                    .clone()
                    .unwrap_or_else(|| "agent reported error".into())
            } else if code == Some(2) {
                "claude exited 2 (blocking error: auth, permission, or limits) \
                 without reporting a budget-ceiling result"
                    .to_string()
            } else if self.saw_result {
                format!("claude exited with code {code:?} after a success result")
            } else {
                format!("claude exited with code {code:?} without a result line")
            }),
            _ => None,
        };
        RunOutcome {
            stop,
            result: self.result,
            error,
            usage: self.usage,
            session_ref: self.session_ref,
            terminal_session_ref: self.terminal_session_ref,
            usage_observed: self.saw_usage,
            output_observed: false,
            resume_failure: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn terminal_result_session_id_backfills_persisted_resume_ref() {
        let mut parser = ClaudeParser::new(false);
        let _init_events = parser.parse_line(r#"{"type":"system","subtype":"init"}"#);
        let _result_events = parser.parse_line(
            r#"{"type":"result","subtype":"success","is_error":false,"result":"done","session_id":"terminal-thread","usage":{"input_tokens":1,"output_tokens":1}}"#,
        );
        let outcome = Box::new(parser).finish(Some(ExitStatus::from_raw(0)), false);
        assert_eq!(outcome.session_ref.as_deref(), Some("terminal-thread"));
        assert_eq!(
            outcome.terminal_session_ref.as_deref(),
            Some("terminal-thread")
        );

        let temp = tempfile::tempdir().unwrap();
        let ledger = crate::ledger::Ledger::open(&temp.path().join("ledger.db")).unwrap();
        let task_id = ledger
            .add_task("terminal id", "spec", "impl", "low", &[], "none")
            .unwrap();
        let prior = ledger
            .start_review_run(task_id, AgentKind::Claude, Some("opus"))
            .unwrap();
        ledger.finish_run(prior, &outcome, 1).unwrap();
        let next = ledger
            .start_review_run(task_id, AgentKind::Claude, Some("opus"))
            .unwrap();
        let resumable = ledger
            .last_run_ref(task_id, "review", Some("claude"), next)
            .unwrap()
            .unwrap();
        assert_eq!(resumable.session_ref.as_deref(), Some("terminal-thread"));
    }

    #[test]
    fn resume_not_found_classifier_accepts_only_claudes_exact_stderr_line() {
        const REQUESTED: &str = "00000000-0000-0000-0000-00000000dead";
        const REAL_CLI_STDERR: &str =
            "No conversation found with session ID: 00000000-0000-0000-0000-00000000dead\n";
        assert!(crate::executor::exact_session_not_found(
            AgentKind::Claude,
            REQUESTED,
            0,
            REAL_CLI_STDERR.len(),
            REAL_CLI_STDERR,
        ));
        assert!(!crate::executor::exact_session_not_found(
            AgentKind::Claude,
            REQUESTED,
            0,
            "warning: No conversation found with session ID: 00000000-0000-0000-0000-00000000dead"
                .len(),
            "warning: No conversation found with session ID: 00000000-0000-0000-0000-00000000dead",
        ));
    }

    #[test]
    fn resume_not_found_classifier_rejects_probe_line_with_extra_stderr_or_suffix() {
        const REQUESTED: &str = "00000000-0000-0000-0000-00000000dead";
        const PROBE_LINE: &str =
            "No conversation found with session ID: 00000000-0000-0000-0000-00000000dead";
        for stderr in [
            format!("other stderr\n{PROBE_LINE}"),
            format!("{PROBE_LINE}\nother stderr"),
            format!("{PROBE_LINE} suffix"),
        ] {
            assert!(!crate::executor::exact_session_not_found(
                AgentKind::Claude,
                REQUESTED,
                0,
                stderr.len(),
                &stderr,
            ));
        }
        assert!(!crate::executor::exact_session_not_found(
            AgentKind::Claude,
            REQUESTED,
            1,
            PROBE_LINE.len(),
            PROBE_LINE,
        ));
    }

    #[test]
    fn resume_not_found_rejects_whitespace_stdout() {
        const REQUESTED: &str = "00000000-0000-0000-0000-00000000dead";
        const PROBE_LINE: &str =
            "No conversation found with session ID: 00000000-0000-0000-0000-00000000dead\n";
        assert!(!crate::executor::exact_session_not_found(
            AgentKind::Claude,
            REQUESTED,
            1,
            PROBE_LINE.len(),
            PROBE_LINE,
        ));
    }

    #[test]
    fn resume_not_found_rejects_probe_line_after_truncated_stderr() {
        const REQUESTED: &str = "00000000-0000-0000-0000-00000000dead";
        const PROBE_LINE: &str =
            "No conversation found with session ID: 00000000-0000-0000-0000-00000000dead\n";
        assert!(!crate::executor::exact_session_not_found(
            AgentKind::Claude,
            REQUESTED,
            0,
            8 * 1024 + PROBE_LINE.len(),
            PROBE_LINE,
        ));
    }

    #[test]
    fn build_resume_args_carries_the_session_id_and_the_new_turn() {
        let driver = ClaudeDriver::new();
        let args = driver.build_resume_args("sess-42", "the fixes landed", &Budget::default());

        assert_eq!(args[0], "-p");
        assert_eq!(args[1], "the fixes landed", "the turn text is the prompt");
        assert_eq!(
            resume_ids(&args),
            vec!["sess-42"],
            "exactly one --resume, carrying the recorded id: {args:?}"
        );
    }

    /// The resume argv must be the fresh argv plus the one flag — every cap
    /// (`--max-turns`, `--max-budget-usd`), the permission mode and the
    /// system prompt ride the resumed run unchanged.
    #[test]
    fn build_resume_args_is_build_args_plus_one_resume_flag() {
        let driver = ClaudeDriver::new();
        let budget = Budget {
            max_turns: Some(3),
            max_budget_usd: Some(2.5),
            ..Default::default()
        };
        let fresh = driver.build_args("turn text", &budget);
        let resumed = driver.build_resume_args("sess-1", "turn text", &budget);

        assert_eq!(resume_ids(&resumed), vec!["sess-1"]);
        let stripped: Vec<String> = strip_resume(&resumed);
        assert_eq!(stripped, fresh, "resume must add nothing but the flag");
    }

    #[test]
    fn fresh_and_resume_argv_are_explicit_and_disjoint() {
        let driver = ClaudeDriver::new();
        assert!(resume_ids(&driver.build_args("fresh", &Budget::default())).is_empty());
        assert_eq!(
            resume_ids(&driver.build_resume_args("explicit", "turn", &Budget::default())),
            vec!["explicit"]
        );
    }

    fn resume_ids(args: &[String]) -> Vec<String> {
        args.windows(2)
            .filter(|pair| pair[0] == "--resume")
            .map(|pair| pair[1].clone())
            .collect()
    }

    fn strip_resume(args: &[String]) -> Vec<String> {
        let mut out = Vec::new();
        let mut skip = 0;
        for (i, arg) in args.iter().enumerate() {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            if arg == "--resume" && i + 1 < args.len() {
                skip = 1;
                continue;
            }
            out.push(arg.clone());
        }
        out
    }
}
