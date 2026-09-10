//! Opt-in, collection-scoped storage over the existing authenticated Bus.
//! The config is operator-owned; neither filesystem roots nor identities come
//! from RPC arguments. This pilot has no delete or public publication verb.
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use cosmix_client::{IncomingCommand, NodedClient};
use cosmix_files::object_store::ObjectStore;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Semaphore;

const MAX_REQUEST: usize = 512 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    root: PathBuf,
    max_bytes: u64,
    max_staging_bytes: u64,
    collections: BTreeMap<String, Access>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Access {
    #[serde(default)]
    readers: Vec<String>,
    #[serde(default)]
    writers: Vec<String>,
}

pub(super) struct Endpoint {
    config: Config,
    store: Mutex<ObjectStore>,
    capacity: Arc<Semaphore>,
}

impl Endpoint {
    pub(super) fn load(public_roots: &[PathBuf]) -> Result<Option<Arc<Self>>, String> {
        let Some(path) = std::env::var_os("COSMIX_STORE_CONFIG") else {
            return Ok(None);
        };
        let bytes = std::fs::read(&path).map_err(|e| format!("storage config: {e}"))?;
        if bytes.len() > 64 * 1024 {
            return Err("storage config exceeds 64 KiB".into());
        }
        let config: Config =
            serde_json::from_slice(&bytes).map_err(|e| format!("storage config: {e}"))?;
        if !config.root.is_absolute()
            || config.collections.is_empty()
            || config.collections.len() > 64
        {
            return Err("storage requires an absolute private root and 1..64 collections".into());
        }
        for (collection, access) in &config.collections {
            if collection.is_empty()
                || collection.len() > 64
                || !collection
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
            {
                return Err("invalid storage collection".into());
            }
            if access.readers.len() + access.writers.len() > 64
                || access
                    .readers
                    .iter()
                    .chain(&access.writers)
                    .any(|p| !valid_node_identity(p))
            {
                return Err(
                    "storage ACL requires node:<name> authenticated node identities".into(),
                );
            }
        }
        // Require operator provisioning of the root. Never create a directory
        // on behalf of a misspelled config before checking the static boundary.
        let root = config
            .root
            .canonicalize()
            .map_err(|e| format!("storage root: {e}"))?;
        for public in public_roots {
            let public = public
                .canonicalize()
                .map_err(|e| format!("public root: {e}"))?;
            if root.starts_with(&public) || public.starts_with(&root) {
                return Err("storage root must be separate from every public docroot".into());
            }
        }
        let store = ObjectStore::open(root, config.max_bytes, config.max_staging_bytes)?;
        Ok(Some(Arc::new(Self {
            config,
            store: Mutex::new(store),
            capacity: Arc::new(Semaphore::new(1)),
        })))
    }

    fn authorize(&self, from: &str, op: &str, args: &Value) -> Result<String, &'static str> {
        let write = match op {
            "object.begin" | "object.chunk" | "object.commit" | "object.abort"
            | "snapshot.commit" => true,
            "object.read" | "snapshot.get" | "snapshot.list" => false,
            _ => return Err("unknown_operation"),
        };
        let collection = args
            .get("collection")
            .and_then(Value::as_str)
            .ok_or("missing_collection")?;
        let access = self
            .config
            .collections
            .get(collection)
            .ok_or("auth_denied")?;
        if from.is_empty()
            || !(access.writers.iter().any(|p| p == from)
                || (!write && access.readers.iter().any(|p| p == from)))
        {
            return Err("auth_denied");
        }
        Ok(collection.to_owned())
    }

    /// Acquire before spawning: no unbounded task queue behind disk I/O.
    pub(super) async fn start(self: &Arc<Self>, cmd: IncomingCommand, client: Arc<NodedClient>) {
        let op = cmd
            .command
            .strip_prefix("webd.store.")
            .unwrap_or("")
            .to_owned();
        let identity = authenticated_node(&cmd).unwrap_or_default();
        let collection = match self.authorize(&identity, &op, &cmd.args) {
            Ok(c) => c,
            Err(error) => {
                reply(&client, &cmd, Err(error.into())).await;
                return;
            }
        };
        if cmd.body.len() > MAX_REQUEST || cmd.args.to_string().len() > MAX_REQUEST {
            reply(&client, &cmd, Err("request_too_large".into())).await;
            return;
        }
        let Ok(permit) = self.capacity.clone().try_acquire_owned() else {
            reply(
                &client,
                &cmd,
                Err("store_busy: retry after the current request completes".into()),
            )
            .await;
            return;
        };
        let endpoint = self.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let args = cmd.args.clone();
            let outcome = tokio::task::spawn_blocking(move || {
                endpoint
                    .store
                    .lock()
                    .map_err(|_| "store lock poisoned".to_string())?
                    .execute(&collection, &op, &args)
            })
            .await
            .unwrap_or_else(|_| Err("storage worker failed".into()));
            reply(&client, &cmd, outcome).await;
        });
    }
}

fn valid_node_identity(identity: &str) -> bool {
    identity.strip_prefix("node:").is_some_and(|name| {
        !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
    })
}

// Reserved headers are stripped from producers and stamped by the receiving
// broker only for D2-admitted direct mesh delivery. Unproven reverse delivery
// and local service-name registration do not grant collection access.
fn authenticated_node(cmd: &IncomingCommand) -> Option<String> {
    if cmd.header("broker_origin") != Some("mesh") {
        return None;
    }
    let identity = format!("node:{}", cmd.header("broker_peer")?);
    valid_node_identity(&identity).then_some(identity)
}

pub(super) async fn reply(
    client: &NodedClient,
    cmd: &IncomingCommand,
    result: Result<Value, String>,
) {
    let (rc, body) = match result {
        Ok(value) => (0, value.to_string()),
        Err(error) => (10, json!({"error": error}).to_string()),
    };
    if let Err(error) = client.respond(cmd, rc, &body).await {
        tracing::warn!(%error, "storage response failed; caller may retry idempotently");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acl_separates_collection_and_read_write_and_ignores_claimed_identity() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = Endpoint {
            config: Config {
                root: dir.path().into(),
                max_bytes: 1024,
                max_staging_bytes: 1024,
                collections: BTreeMap::from([(
                    "coast".into(),
                    Access {
                        readers: vec!["node:reader".into()],
                        writers: vec!["node:writer".into()],
                    },
                )]),
            },
            store: Mutex::new(ObjectStore::open(dir.path().into(), 1024, 1024).unwrap()),
            capacity: Arc::new(Semaphore::new(1)),
        };
        let args = json!({"collection":"coast", "from":"node:writer"});
        assert!(endpoint.authorize("", "snapshot.list", &args).is_err());
        assert!(
            endpoint
                .authorize("node:other", "object.begin", &args)
                .is_err()
        );
        assert!(
            endpoint
                .authorize("node:reader", "object.begin", &args)
                .is_err()
        );
        assert!(
            endpoint
                .authorize("node:reader", "snapshot.list", &args)
                .is_ok()
        );
        assert!(
            endpoint
                .authorize("node:writer", "object.begin", &args)
                .is_ok()
        );
        assert!(
            endpoint
                .authorize(
                    "node:writer",
                    "snapshot.list",
                    &json!({"collection":"other"})
                )
                .is_err()
        );
        assert!(endpoint.authorize("node:writer", "delete", &args).is_err());
    }

    #[test]
    fn node_identity_requires_broker_provenance_not_body_or_service_name() {
        let mut cmd = IncomingCommand {
            from: "node:writer".into(),
            command: String::new(),
            id: None,
            args: json!({"broker_peer":"writer"}),
            body: String::new(),
            headers: BTreeMap::new(),
        };
        assert!(authenticated_node(&cmd).is_none());
        cmd.headers.insert("broker_origin".into(), "mesh".into());
        assert!(authenticated_node(&cmd).is_none());
        cmd.headers.insert("broker_peer".into(), "writer".into());
        assert_eq!(authenticated_node(&cmd).as_deref(), Some("node:writer"));
        cmd.headers.insert("broker_origin".into(), "local".into());
        assert!(authenticated_node(&cmd).is_none());
    }
}
