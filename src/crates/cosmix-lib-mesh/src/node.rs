//! Mesh node identity and discovery.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A node on the cosmix WireGuard mesh.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshNode {
    pub id: Uuid,
    pub name: String,
    pub wg_pubkey: String,
    pub wg_endpoint: Option<String>,
    pub jmap_url: Option<String>,
    pub mesh_ip: String,
}
