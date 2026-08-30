pub mod claude;
pub mod codex;

use anyhow::{Context, Result};

use crate::executor::{AgentKind, Executor};

/// Marker context attached to every `build` error: the RUNG itself refused
/// to start (missing vendor credentials, unenforceable budget). Dispatch
/// treats these as infrastructure refusals with a short backoff; they never
/// charge or advance the task's quality ladder.
#[derive(Debug)]
pub struct RungRefusal;

impl std::fmt::Display for RungRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("rung driver refused to start")
    }
}

/// Whether the operator has deliberately opted this host into METERED API
/// billing (`FOREMAN_ALLOW_API_BILLING=1`).
///
/// Default OFF, and deliberately the one place foreman is closed-by-default
/// rather than open: every other gate here guards correctness, but a
/// vendor API key in the environment silently outranks a subscription login
/// and converts an unattended fleet's whole day into a metered bill — with
/// no signal in the ledger, because the CLIs report list-price cost either
/// way. "Default open" belongs to capability, not to someone's credit card.
pub fn api_billing_allowed() -> bool {
    std::env::var("FOREMAN_ALLOW_API_BILLING")
        .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

/// Vendor API keys that turn a subscription login into a metered bill.
/// Scrubbed from EVERY agent session, not just the matching lane: an agent
/// shells out to other vendors' CLIs routinely (a claude session running
/// `codex exec` is ordinary here), so the honest rule is that no agent
/// session carries any vendor API key at all. Each CLI then authenticates
/// with its own subscription login, which is what the operator pays for.
const METERED_KEYS: &[&str] = &["ANTHROPIC_API_KEY", "OPENAI_API_KEY"];

/// Strip the metered-billing credentials from a session about to spawn,
/// unless the operator opted in via [`api_billing_allowed`].
pub(crate) fn scrub_metered_keys(cmd: &mut std::process::Command) {
    if api_billing_allowed() {
        return;
    }
    for key in METERED_KEYS {
        cmd.env_remove(key);
    }
}

/// Resolve the Z.ai token: `ZAI_API_KEY` directly, or lazily from the
/// env-format file named by `FOREMAN_ZAI_KEY_FILE` (`ZAI_API_KEY=…` line).
/// The file route is the fleet default: putting the key in the DISPATCH
/// process's environment (systemd EnvironmentFile) hands it to every child
/// of every rung — a claude/codex task that needs no Z.ai key could simply
/// `printenv` it under bypassPermissions. Read here, it is injected into
/// the GLM child's environment only.
fn zai_token() -> Result<String> {
    if let Ok(t) = std::env::var("ZAI_API_KEY") {
        return Ok(t);
    }
    if let Ok(path) = std::env::var("FOREMAN_ZAI_KEY_FILE") {
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("reading FOREMAN_ZAI_KEY_FILE {path}"))?;
        return content
            .lines()
            .find_map(|l| l.strip_prefix("ZAI_API_KEY="))
            .map(|v| v.trim().to_string())
            .with_context(|| format!("no ZAI_API_KEY= line in {path}"));
    }
    anyhow::bail!(
        "glm driver needs ZAI_API_KEY in the environment or FOREMAN_ZAI_KEY_FILE \
         pointing at an env file holding it"
    )
}

/// Build the driver for `kind` with the shared CLI knobs applied. The GLM
/// driver is the Claude driver pointed at Z.ai — it needs `ZAI_API_KEY` in
/// the environment and must never be handed secret-bearing paths (Chinese
/// data law applies to the Z.ai endpoint; policy tag `provider=zai`).
///
/// `FOREMAN_CLAUDE_BIN` / `FOREMAN_CODEX_BIN` override the vendor binary —
/// for pinning a known-good CLI version, and for fixture-driven smoke runs.
/// They and the sibling-repository bind set come from the invocation policy
/// snapshot; this function never re-reads those environment variables while
/// a dispatch sweep is in progress. Credentials, the API-billing interlock
/// and the API-billing interlock remain local launch inputs rather than fleet
/// policy, so their environment reads stay here deliberately.
pub fn build(
    kind: AgentKind,
    model: Option<String>,
    permission_mode: Option<String>,
    extra_args: Vec<String>,
    policy: &crate::config::FleetPolicy,
    hook_mounts: Option<crate::sandbox::HookMounts>,
) -> Result<Box<dyn Executor>> {
    Ok(match kind {
        AgentKind::Claude => Box::new(
            claude::ClaudeDriver::new()
                .with_program(&policy.claude_bin.value)
                .with_model(model)
                .with_permission_mode(permission_mode)
                .with_sibling_repos(policy.sibling_repos.value.clone())
                .with_hook_mounts(hook_mounts)
                .with_extra_args(extra_args),
        ),
        AgentKind::Glm => {
            let token = zai_token()?;
            Box::new(
                claude::ClaudeDriver::glm(&token)
                    .with_program(&policy.claude_bin.value)
                    .with_model(model)
                    .with_permission_mode(permission_mode)
                    .with_sibling_repos(policy.sibling_repos.value.clone())
                    .with_hook_mounts(hook_mounts)
                    .with_extra_args(extra_args),
            )
        }
        AgentKind::Codex => Box::new(
            codex::CodexDriver::new()
                .with_program(&policy.codex_bin.value)
                .with_sibling_repos(policy.sibling_repos.value.clone())
                .with_model(model)
                .with_extra_args(extra_args),
        ),
    })
}
