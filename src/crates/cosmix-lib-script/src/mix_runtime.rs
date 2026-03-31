//! Mix scripting runtime — bridges mix-core with cosmix AMP.
//!
//! Provides an `AmpHandler` implementation backed by `HubClient` and
//! a factory function to create a Mix evaluator pre-loaded with AMP
//! context variables.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use mix_core::evaluator::{AmpHandler, Evaluator, SharedBuf};
use mix_core::error::MixResult;
use mix_core::json::{json_to_mix, mix_to_json};
use mix_core::value::Value;

use crate::types::ScriptResult;

/// AMP handler that routes Mix `send`/`emit`/`port_exists` to the hub.
struct HubAmpHandler {
    hub: Arc<cosmix_client::HubClient>,
}

impl AmpHandler for HubAmpHandler {
    fn send<'a>(
        &'a self,
        target: &'a str,
        command: &'a str,
        args: &'a Value,
    ) -> Pin<Box<dyn Future<Output = MixResult<(u8, Value)>> + 'a>> {
        Box::pin(async move {
            let json_args = mix_to_json(args);
            match self.hub.call(target, command, json_args).await {
                Ok(resp) => Ok((0, json_to_mix(resp))),
                Err(e) => Ok((10, Value::String(e.to_string()))),
            }
        })
    }

    fn emit<'a>(
        &'a self,
        target: &'a str,
        command: &'a str,
        args: &'a Value,
    ) -> Pin<Box<dyn Future<Output = MixResult<()>> + 'a>> {
        Box::pin(async move {
            let json_args = mix_to_json(args);
            let _ = self.hub.send(target, command, json_args).await;
            Ok(())
        })
    }

    fn port_exists<'a>(
        &'a self,
        target: &'a str,
    ) -> Pin<Box<dyn Future<Output = MixResult<bool>> + 'a>> {
        Box::pin(async move {
            match self.hub.list_services().await {
                Ok(services) => Ok(services.iter().any(|s| s == target)),
                Err(_) => Ok(false),
            }
        })
    }
}

/// Create a Mix evaluator wired to the hub with AMP context variables.
///
/// Injects `$SERVICE` and any app-provided variables as Mix globals.
/// The returned `SharedBuf` pair captures stdout/stderr for `ScriptResult`.
pub fn create_evaluator(
    hub: Arc<cosmix_client::HubClient>,
    service_name: &str,
    app_vars: &HashMap<String, String>,
) -> (Evaluator, SharedBuf, SharedBuf) {
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();

    let mut eval = Evaluator::with_output(
        Box::new(stdout.clone()),
        Box::new(stderr.clone()),
    );

    // Wire AMP IPC
    eval.set_amp_handler(Box::new(HubAmpHandler { hub }));

    // Inject context variables
    eval.set_global("SERVICE", Value::String(service_name.to_string()));
    for (k, v) in app_vars {
        eval.set_global(k, Value::String(v.clone()));
    }

    (eval, stdout, stderr)
}

/// Execute a Mix script source string and return a `ScriptResult`.
pub async fn execute_mix(
    source: &str,
    hub: Arc<cosmix_client::HubClient>,
    service_name: &str,
    app_vars: &HashMap<String, String>,
) -> ScriptResult {
    // Parse
    let mut lexer = mix_core::lexer::Lexer::new(source);
    let tokens = match lexer.tokenize() {
        Ok(t) => t,
        Err(e) => {
            return ScriptResult {
                rc: 10,
                body: None,
                error: Some(format!("Parse error: {e}")),
            };
        }
    };
    let mut parser = mix_core::parser::Parser::new(tokens);
    let stmts = match parser.parse_program() {
        Ok(s) => s,
        Err(e) => {
            return ScriptResult {
                rc: 10,
                body: None,
                error: Some(format!("Parse error: {e}")),
            };
        }
    };

    // Execute
    let (mut eval, stdout, _stderr) = create_evaluator(hub, service_name, app_vars);

    match eval.execute(&stmts).await {
        Ok(_) => {
            let output = stdout.to_string_lossy();
            ScriptResult {
                rc: 0,
                body: if output.is_empty() { None } else { Some(output) },
                error: None,
            }
        }
        Err(e) => {
            let output = stdout.to_string_lossy();
            ScriptResult {
                rc: 10,
                body: if output.is_empty() { None } else { Some(output) },
                error: Some(e.to_string()),
            }
        }
    }
}
