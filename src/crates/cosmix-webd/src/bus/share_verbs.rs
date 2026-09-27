//! Mesh-open catalogue verbs. JSON arguments only; no transport-header fallback.
use crate::{NodeState, VhostState, shares};
use cosmix_client::IncomingCommand;
use serde_json::{Value, json};
use std::sync::Arc;

pub fn handles(command: &str) -> bool {
    matches!(
        command,
        "webd.share.create" | "webd.share.list" | "webd.share.revoke" | "webd.media.ref"
    )
}
pub fn vhost(node: &NodeState, args: &Value) -> Result<Arc<VhostState>, String> {
    let name = args["vhost"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("invalid_arguments: vhost required")?;
    node.vhost_for_host(&name.to_ascii_lowercase())
        .ok_or_else(|| "not_found".into())
}
pub async fn dispatch(node: &Arc<NodeState>, cmd: &IncomingCommand) -> (u8, String) {
    let result = execute(node, &cmd.command, cmd.args.clone()).await;
    match result {
        Ok(value) => (0, value.to_string()),
        Err(reason) => (10, json!({"error": reason}).to_string()),
    }
}
async fn execute(node: &NodeState, command: &str, mut args: Value) -> Result<Value, String> {
    let _permit = node.share_runtime.admit()?;
    let vhost = vhost(node, &args)?;
    args.as_object_mut()
        .ok_or("invalid_arguments: JSON object required")?
        .remove("vhost");
    match command {
        "webd.share.create" => {
            let request = serde_json::from_value(args)
                .map_err(|_| "invalid_arguments: invalid share.create arguments")?;
            shares::create(node, &vhost, request).await
        }
        "webd.share.list" => {
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Args {
                account: String,
                after: Option<String>,
                limit: Option<usize>,
            }
            let args: Args = serde_json::from_value(args)
                .map_err(|_| "invalid_arguments: invalid share.list arguments")?;
            shares::list(
                &vhost,
                &args.account,
                args.after.as_deref(),
                args.limit.unwrap_or(100),
            )
            .await
        }
        "webd.share.revoke" => {
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Args {
                account: String,
                token: String,
            }
            let args: Args = serde_json::from_value(args)
                .map_err(|_| "invalid_arguments: invalid share.revoke arguments")?;
            shares::revoke(&vhost, &args.account, &args.token).await
        }
        "webd.media.ref" => {
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Args {
                id: i64,
            }
            let args: Args = serde_json::from_value(args)
                .map_err(|_| "invalid_arguments: media.ref requires integer id")?;
            let reference = crate::media::media_ref(node, &vhost, args.id).await?;
            serde_json::to_value(reference)
                .map_err(|_| "internal: reference serialisation failed".into())
        }
        _ => Err("invalid_arguments: unknown share verb".into()),
    }
}
