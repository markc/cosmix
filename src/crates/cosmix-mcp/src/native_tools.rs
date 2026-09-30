//! Semantic desktop tools are translations of existing owning-service Bus
//! contracts. Explicit service targets are never resolved by caption or focus.
//! Admission, canonicalisation and effects stay in CTK/compositor services.

use cosmix_bus::PortReply;
use rmcp::{
    handler::server::wrapper::{Json, Parameters},
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::CosmixMcp;

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct NativeObservation {
    service: String,
    command: String,
    /// Owning-service response; accepted/applied acknowledgements must not be
    /// interpreted as downstream completion. Query observable state to verify.
    result: Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ServiceTarget {
    /// Exact process-scoped service, optionally including native Bus routing.
    service: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ControlTarget {
    service: String,
    target: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ControlWrite {
    service: String,
    target: String,
    /// Native domain value; CTK validates type, bounds, disabled/busy state.
    value: Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ActionInvocation {
    service: String,
    id: String,
    #[serde(default)]
    args: serde_json::Map<String, Value>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct WindowObservation {
    service: String,
    app_id: Option<String>,
    title: Option<String>,
    title_contains: Option<String>,
    visible: Option<bool>,
    workspace: Option<u64>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct WindowTarget {
    id: u64,
    /// Surface role generation, not a compositor process incarnation token.
    generation: u64,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WindowCondition {
    Mapped,
    Visible,
    Presented,
    Focused,
    Maximized,
    Unmaximized,
    Fullscreen,
    Unfullscreen,
    Unmapped,
    Gone,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct WindowWait {
    service: String,
    window: WindowTarget,
    until: WindowCondition,
    /// Positive bound, at most 55 seconds (below the native client deadline).
    timeout_ms: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ControlWait {
    service: String,
    target: String,
    /// Expected domain value, compared with the owning-service get reply's value.
    value: Value,
    timeout_ms: u64,
}

fn exact_target(value: &str, field: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 255
        || value
            .chars()
            .any(|c| c.is_control() || (field == "service" && c.is_whitespace()))
    {
        Err(format!(
            "{field} must be an explicit nonempty target, at most 255 bytes"
        ))
    } else {
        Ok(())
    }
}

fn wait_bound(timeout_ms: u64) -> Result<std::time::Duration, String> {
    if !(1..=55_000).contains(&timeout_ms) {
        return Err("timeout_ms must be in 1..=55000".into());
    }
    Ok(std::time::Duration::from_millis(timeout_ms))
}

impl CosmixMcp {
    async fn native_observation(
        &self,
        service: String,
        command: &str,
        args: Value,
    ) -> Result<Json<NativeObservation>, String> {
        exact_target(&service, "service")?;
        if serde_json::to_vec(&args).map_err(|e| e.to_string())?.len() > 16_384 {
            return Err("native desktop request exceeds 16384 bytes; nothing was sent".into());
        }
        let noded = self.noded().await?;
        // One attempt only. A lost reply does not establish that work stopped.
        let result = match noded.call_typed(&service, command, args).await {
            Ok(PortReply::Ok { value, .. }) => value,
            Ok(PortReply::AppError { rc, message }) => {
                return Err(format!("application rc={rc}: {message}"));
            }
            Err(error) => {
                return Err(format!(
                    "native request failed: {error}; outcome unknown if delivered; verify effects before retrying"
                ));
            }
        };
        if serde_json::to_vec(&result)
            .map_err(|e| e.to_string())?
            .len()
            > 1024 * 1024
        {
            return Err("native desktop response exceeds 1 MiB".into());
        }
        Ok(Json(NativeObservation {
            service,
            command: command.into(),
            result,
        }))
    }
}

#[tool_router(router = native_tool_router, vis = "pub(crate)")]
impl CosmixMcp {
    /// Describe one explicitly targeted CTK application and its native verbs.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true
    ))]
    async fn app_describe(
        &self,
        Parameters(p): Parameters<ServiceTarget>,
    ) -> Result<Json<NativeObservation>, String> {
        self.native_observation(p.service, "app.describe", json!({}))
            .await
    }

    /// List semantic controls from an explicit CTK service. This is control
    /// metadata, not a complete accessibility tree or screen geometry snapshot.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true
    ))]
    async fn app_controls_list(
        &self,
        Parameters(p): Parameters<ServiceTarget>,
    ) -> Result<Json<NativeObservation>, String> {
        self.native_observation(p.service, "app.controls.list", json!({}))
            .await
    }

    /// Read the native domain value of one explicitly named application control.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true
    ))]
    async fn app_control_get(
        &self,
        Parameters(p): Parameters<ControlTarget>,
    ) -> Result<Json<NativeObservation>, String> {
        exact_target(&p.target, "target")?;
        self.native_observation(p.service, "app.controls.get", json!({"target": p.target}))
            .await
    }

    /// Drive a semantic CTK control. CTK owns admission, canonical values and
    /// busy/disabled checks. A successful reply means local dispatch, not
    /// downstream completion. Read or wait for the resulting state to verify.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = false
    ))]
    async fn app_control_set(
        &self,
        Parameters(p): Parameters<ControlWrite>,
    ) -> Result<Json<NativeObservation>, String> {
        exact_target(&p.target, "target")?;
        self.native_observation(
            p.service,
            "app.controls.set",
            json!({"target": p.target, "value": p.value}),
        )
        .await
    }

    /// List actions on an exact application service. Existing CTK caller
    /// admission applies to action discovery as well as invocation.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true
    ))]
    async fn app_actions_list(
        &self,
        Parameters(p): Parameters<ServiceTarget>,
    ) -> Result<Json<NativeObservation>, String> {
        self.native_observation(p.service, "actions.list", json!({}))
            .await
    }

    /// Invoke a semantic action once on the explicit application service.
    /// CTK validates enabled state, modal barriers, source and argument schema.
    /// accepted:true is an acknowledgement, not proof the action completed.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = false
    ))]
    async fn app_action_invoke(
        &self,
        Parameters(p): Parameters<ActionInvocation>,
    ) -> Result<Json<NativeObservation>, String> {
        exact_target(&p.id, "id")?;
        self.native_observation(
            p.service,
            "action.invoke",
            json!({"id": p.id, "args": p.args}),
        )
        .await
    }

    /// Observe compositor windows and their logical geometry/generations.
    /// Filters restrict observations; they never select a mutation target.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true
    ))]
    async fn desktop_windows(
        &self,
        Parameters(p): Parameters<WindowObservation>,
    ) -> Result<Json<NativeObservation>, String> {
        let mut args = serde_json::Map::new();
        for (key, value) in [
            ("app_id", p.app_id),
            ("title", p.title),
            ("title_contains", p.title_contains),
        ] {
            if let Some(value) = value {
                args.insert(key.into(), Value::String(value));
            }
        }
        if let Some(visible) = p.visible {
            args.insert("visible".into(), json!(visible));
        }
        if let Some(workspace) = p.workspace {
            args.insert("workspace".into(), json!(workspace));
        }
        self.native_observation(p.service, "comp.windows.list", Value::Object(args))
            .await
    }

    /// Wait for an explicitly identified compositor window's observable state.
    /// Generation fences surface role changes; it does not prove process identity.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true
    ))]
    async fn desktop_window_wait(
        &self,
        Parameters(p): Parameters<WindowWait>,
    ) -> Result<Json<NativeObservation>, String> {
        wait_bound(p.timeout_ms)?;
        self.native_observation(
            p.service,
            "comp.window.wait",
            json!({
                "match": p.window, "until": p.until, "timeout_ms": p.timeout_ms
            }),
        )
        .await
    }

    /// Poll one semantic control for a concrete native value. Bounded, read-only
    /// and cancellable between observations; never replays the preceding action.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true
    ))]
    async fn app_control_wait(
        &self,
        Parameters(p): Parameters<ControlWait>,
    ) -> Result<Json<NativeObservation>, String> {
        exact_target(&p.target, "target")?;
        let deadline = tokio::time::Instant::now() + wait_bound(p.timeout_ms)?;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let observation = tokio::time::timeout(
                remaining,
                self.native_observation(
                    p.service.clone(),
                    "app.controls.get",
                    json!({"target": p.target}),
                ),
            )
            .await
            .map_err(|_| {
                "control wait deadline elapsed; desired value was not observed".to_string()
            })??;
            if observation.0.result.get("value") == Some(&p.value) {
                return Ok(observation);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err("control wait deadline elapsed; desired value was not observed".into());
            }
            tokio::time::sleep(
                std::time::Duration::from_millis(250)
                    .min(deadline.saturating_duration_since(tokio::time::Instant::now())),
            )
            .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_targets_and_waits_are_bounded() {
        assert!(exact_target("mixer-ctk-123", "service").is_ok());
        for target in ["", " ", "app\n", "app\0"] {
            assert!(exact_target(target, "service").is_err());
        }
        assert!(wait_bound(0).is_err());
        assert!(wait_bound(55_001).is_err());
        assert!(wait_bound(55_000).is_ok());
    }

    #[test]
    fn semantic_inputs_reject_unknown_fields() {
        assert!(
            serde_json::from_value::<ControlWrite>(json!({
                "service":"app-123", "target":"volume", "value":0.5, "focus":true
            }))
            .is_err()
        );
    }
}
