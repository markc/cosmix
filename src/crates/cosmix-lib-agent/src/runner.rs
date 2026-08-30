use anyhow::Result;
use cosmix_llm::{ChatRequest, ContentBlock, LlmClient, Message, Role};
use tracing::{debug, info, warn};

use crate::session::Session;
use crate::tools::ToolRegistry;
use crate::{DEFAULT_MAX_TOKENS, MAX_ITERATIONS};

#[derive(Debug, Clone)]
pub struct TurnOutcome {
    pub text: String,
    pub iterations: u32,
    pub hit_cap: bool,
}

/// Drive one user-to-assistant turn to completion.
///
/// Appends the user message, calls the LLM, dispatches any requested tools,
/// and loops until the model returns a turn with no tool calls (or we hit
/// `MAX_ITERATIONS`). All messages — user, assistant, and tool results — are
/// appended to `session.messages` in the order Anthropic expects.
pub async fn run_turn(
    session: &mut Session,
    user_text: &str,
    client: &LlmClient,
    tools: &ToolRegistry,
) -> Result<TurnOutcome> {
    session.append(Message::user_text(user_text));

    let schemas = tools.schemas();
    let mut iterations: u32 = 0;
    let mut final_text = String::new();
    let mut hit_cap = false;

    loop {
        if iterations >= MAX_ITERATIONS {
            warn!(session_id = %session.id, "hit MAX_ITERATIONS cap");
            hit_cap = true;
            break;
        }

        let req = {
            let mut r = ChatRequest::new(&session.messages)
                .with_tools(&schemas)
                .with_max_tokens(DEFAULT_MAX_TOKENS);
            if let Some(sys) = session.system.as_deref() {
                r = r.with_system(sys);
            }
            r
        };

        debug!(session_id = %session.id, iteration = iterations, "calling LLM");
        let resp = client.chat(req).await?;
        session.add_usage(resp.usage);

        final_text.clear();
        for block in &resp.content {
            if let ContentBlock::Text { text } = block {
                if !final_text.is_empty() {
                    final_text.push('\n');
                }
                final_text.push_str(text);
            }
        }

        let calls: Vec<(String, String, serde_json::Value)> = resp
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, name, input } => {
                    Some((id.clone(), name.clone(), input.clone()))
                }
                _ => None,
            })
            .collect();

        session.append(resp.into_message());

        if calls.is_empty() {
            break;
        }

        iterations += 1;

        let mut result_blocks = Vec::with_capacity(calls.len());
        for (id, name, input) in calls {
            info!(session_id = %session.id, tool = %name, "dispatching tool");
            match tools.get(&name) {
                Some(tool) => match tool.run(input).await {
                    Ok(out) => result_blocks.push(ContentBlock::ToolResult {
                        tool_use_id: id,
                        content: out,
                        is_error: false,
                    }),
                    Err(e) => {
                        warn!(tool = %name, error = %e, "tool execution error");
                        result_blocks.push(ContentBlock::ToolResult {
                            tool_use_id: id,
                            content: format!("error: {e}"),
                            is_error: true,
                        });
                    }
                },
                None => {
                    warn!(tool = %name, "unknown tool, feeding error back to model");
                    result_blocks.push(ContentBlock::ToolResult {
                        tool_use_id: id,
                        content: format!("error: tool '{name}' is not registered"),
                        is_error: true,
                    });
                }
            }
        }

        session.append(Message {
            role: Role::User,
            content: result_blocks,
        });
    }

    Ok(TurnOutcome {
        text: final_text,
        iterations,
        hit_cap,
    })
}
