//! Script executor — runs script steps sequentially via the hub.

use crate::types::{ScriptContext, ScriptDef, ScriptResult};
use crate::variables::substitute;

/// Execute a script by running each step sequentially via the hub.
///
/// Steps are executed in order. If a step has `store`, the response body
/// is saved as a variable for subsequent steps. Execution aborts on
/// RC >= 10 (error or failure).
pub async fn execute(
    script: &ScriptDef,
    ctx: &mut ScriptContext,
    hub: &cosmix_client::HubClient,
) -> ScriptResult {
    let mut last_body: Option<String> = None;

    for (i, step) in script.steps.iter().enumerate() {
        tracing::debug!(
            "Script '{}' step {}: {} → {}",
            script.script.name,
            i + 1,
            step.command,
            step.to
        );

        // Substitute variables in args
        let args = if let Some(ref template) = step.args {
            let substituted = substitute(template, ctx);
            tracing::info!("Script step {} substituted args: {substituted}", i + 1);
            match serde_json::from_str(&substituted) {
                Ok(val) => val,
                Err(e) => {
                    tracing::error!("Script step {} args parse error: {e}", i + 1);
                    return ScriptResult {
                        rc: 10,
                        body: None,
                        error: Some(format!("Step {} args parse error: {e}", i + 1)),
                    };
                }
            }
        } else {
            serde_json::Value::Null
        };

        // Execute the AMP call
        match hub.call(&step.to, &step.command, args).await {
            Ok(response) => {
                let body = response.to_string();
                tracing::info!("Script step {} response: {body}", i + 1);

                // Store result if requested
                if let Some(ref var_name) = step.store {
                    // Try to extract a meaningful string value:
                    // If response is {"content": "..."}, store the content value
                    // Otherwise store the full JSON string
                    let store_val = if let Some(obj) = response.as_object() {
                        // If there's a single string value, use it directly
                        if obj.len() == 1 {
                            if let Some(val) = obj.values().next() {
                                match val {
                                    serde_json::Value::String(s) => s.clone(),
                                    _ => val.to_string(),
                                }
                            } else {
                                body.clone()
                            }
                        } else if let Some(serde_json::Value::String(s)) = obj.get(var_name.as_str()) {
                            // If there's a field matching the store name, use it
                            s.clone()
                        } else {
                            body.clone()
                        }
                    } else if let Some(s) = response.as_str() {
                        s.to_string()
                    } else {
                        body.clone()
                    };

                    ctx.step_vars.insert(var_name.clone(), store_val);
                }

                last_body = Some(body);
            }
            Err(e) => {
                tracing::error!(
                    "Script '{}' step {} failed: {e}",
                    script.script.name,
                    i + 1
                );
                return ScriptResult {
                    rc: 10,
                    body: last_body,
                    error: Some(format!("Step {} ({}) failed: {e}", i + 1, step.command)),
                };
            }
        }
    }

    ScriptResult {
        rc: 0,
        body: last_body,
        error: None,
    }
}
