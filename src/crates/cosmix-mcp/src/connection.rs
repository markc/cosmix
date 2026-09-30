//! Lazy native Bus connection with live-state observation. Reconnection only
//! acquires a client for a NEW request; an in-flight command is never replayed.

use cosmix_client::NodedClient;
use std::sync::{Arc, Mutex};

pub(crate) struct BrokerConnection {
    client: Mutex<Option<Arc<NodedClient>>>,
    connecting: tokio::sync::Mutex<()>,
    name: String,
    started_at: String,
}

impl BrokerConnection {
    pub(crate) fn new() -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        Self {
            client: Mutex::new(None),
            connecting: tokio::sync::Mutex::new(()),
            name: format!("mcp-{}-{nonce:x}", std::process::id()),
            started_at: cosmix_buildinfo::now_rfc3339(),
        }
    }

    fn live_client(&self) -> Option<Arc<NodedClient>> {
        self.client
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .filter(|client| client.is_connected())
            .cloned()
    }

    pub(crate) fn is_connected(&self) -> bool {
        self.live_client().is_some()
    }

    pub(crate) async fn get(&self) -> Result<Arc<NodedClient>, String> {
        if let Some(client) = self.live_client() {
            return Ok(client);
        }
        let _connecting = self.connecting.lock().await;
        if let Some(client) = self.live_client() {
            return Ok(client);
        }
        let previous = self.client.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(previous) = previous {
            previous.close().await;
        }
        let build = cosmix_buildinfo::build_info!();
        let provenance = cosmix_bus::RegisterProvenance {
            binary: Some("cosmix-mcp".into()),
            version: Some(build.version.into()),
            git_sha: Some(build.git_sha.into()),
            git_dirty: Some(build.git_dirty),
            build_time: Some(build.build_time.into()),
            pid: Some(std::process::id()),
            started_at: Some(self.started_at.clone()),
            ..Default::default()
        };
        let client = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            cosmix_config::client_helpers::connect_default_with_provenance(&self.name, provenance),
        )
        .await
        .map_err(|_| {
            "broker connection/registration deadline elapsed; nothing was sent".to_string()
        })?
        .map_err(|e| format!("broker connect failed: {e}; nothing was sent"))?;
        let client = Arc::new(client);
        // Registration satisfies CTK's existing named-caller lane; it is not
        // principal authentication. Drain addressed requests so registration
        // does not introduce an unconsumed incoming-command queue. A Weak
        // reference lets cache replacement release the old connection owner.
        if let Some(mut incoming) = client.incoming_async().await {
            let weak = Arc::downgrade(&client);
            tokio::spawn(async move {
                while let Some(command) = incoming.recv().await {
                    let Some(client) = weak.upgrade() else {
                        break;
                    };
                    let _ = tokio::time::timeout(
                        std::time::Duration::from_secs(1),
                        client.respond(&command, 10, "MCP bridge has no application command port"),
                    )
                    .await;
                }
            });
        }
        *self.client.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&client));
        Ok(client)
    }
}
