use crate::utils::Cache;
use mastodon_async::prelude::StatusId;
use mastodon_async::{Mastodon, NewStatus};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::mpsc;

pub static PORT_RX: OnceLock<
    Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<cosmix_port::PortEvent>>>,
> = OnceLock::new();

pub fn start_port(
    mastodon: Mastodon,
    cache: Arc<Mutex<Cache>>,
    notifier: mpsc::UnboundedSender<cosmix_port::PortEvent>,
) -> Option<cosmix_port::PortHandle> {
    let m1 = mastodon.clone();
    let m2 = mastodon.clone();
    let m3 = mastodon.clone();
    let m4 = mastodon.clone();
    let m5 = mastodon.clone();
    let c1 = cache.clone();
    let c2 = cache.clone();

    let port = cosmix_port::Port::new("cosmix-toot")
        .events(notifier)
        .command("status", "Get app status (instance, auth, cache stats)", move |_| {
            let c = c1.lock().unwrap();
            let logged_in = !m1.data.token.is_empty();
            let instance = m1.data.base.to_string();
            Ok(serde_json::json!({
                "instance": instance,
                "logged_in": logged_in,
                "cached_statuses": c.statuses.len(),
                "cached_notifications": c.notifications.len(),
                "cached_images": c.handles.len(),
            }))
        })
        .command("timeline", "Get cached home timeline status IDs", move |_| {
            let c = c2.lock().unwrap();
            let ids: Vec<String> = c.statuses.keys().cloned().collect();
            Ok(serde_json::json!({
                "count": ids.len(),
                "ids": ids,
            }))
        })
        .command("post", "Post a new status (args: string or {\"status\": \"...\", \"visibility\": \"public|unlisted|private|direct\"})", move |args| {
            let (text, visibility) = if let Some(s) = args.as_str() {
                (s.to_string(), None)
            } else {
                let text = args.get("status")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("missing 'status' field"))?
                    .to_string();
                let vis = args.get("visibility").and_then(|v| v.as_str()).map(String::from);
                (text, vis)
            };
            let mastodon = m2.clone();
            let rt = tokio::runtime::Handle::try_current()
                .or_else(|_| {
                    let rt = tokio::runtime::Runtime::new()?;
                    Ok::<_, anyhow::Error>(rt.handle().clone())
                })?;
            let result = rt.block_on(async {
                let mut new_status = NewStatus::default();
                new_status.status = Some(text);
                if let Some(vis) = visibility {
                    new_status.visibility = Some(match vis.as_str() {
                        "direct" => mastodon_async::prelude::Visibility::Direct,
                        "private" => mastodon_async::prelude::Visibility::Private,
                        "unlisted" => mastodon_async::prelude::Visibility::Unlisted,
                        _ => mastodon_async::prelude::Visibility::Public,
                    });
                }
                mastodon.new_status(new_status).await
            })?;
            Ok(serde_json::json!({
                "id": result.id.to_string(),
                "url": result.url,
            }))
        })
        .command("favorite", "Favorite a status by ID (args: status ID string)", move |args| {
            let id = args.as_str()
                .ok_or_else(|| anyhow::anyhow!("expected status ID string"))?;
            let mastodon = m3.clone();
            let status_id: StatusId = id.to_string().into();
            let rt = tokio::runtime::Handle::try_current()
                .or_else(|_| {
                    let rt = tokio::runtime::Runtime::new()?;
                    Ok::<_, anyhow::Error>(rt.handle().clone())
                })?;
            let result = rt.block_on(async {
                mastodon.favourite(&status_id).await
            })?;
            Ok(serde_json::json!({
                "id": result.id.to_string(),
                "favourited": result.favourited,
            }))
        })
        .command("boost", "Boost/reblog a status by ID (args: status ID string)", move |args| {
            let id = args.as_str()
                .ok_or_else(|| anyhow::anyhow!("expected status ID string"))?;
            let mastodon = m4.clone();
            let status_id: StatusId = id.to_string().into();
            let rt = tokio::runtime::Handle::try_current()
                .or_else(|_| {
                    let rt = tokio::runtime::Runtime::new()?;
                    Ok::<_, anyhow::Error>(rt.handle().clone())
                })?;
            let result = rt.block_on(async {
                mastodon.reblog(&status_id).await
            })?;
            Ok(serde_json::json!({
                "id": result.id.to_string(),
                "reblogged": result.reblogged,
            }))
        })
        .command("notifications", "Get notification count", move |_| {
            let logged_in = !m5.data.token.is_empty();
            Ok(serde_json::json!({
                "logged_in": logged_in,
            }))
        })
        .standard_help()
        .standard_info("Cosmix Toot", env!("CARGO_PKG_VERSION"))
        .standard_activate();

    match port.start() {
        Ok(handle) => {
            tracing::info!("Cosmix port 'cosmix-toot' started");
            Some(handle)
        }
        Err(e) => {
            tracing::warn!("Failed to start cosmix port: {e}");
            None
        }
    }
}
