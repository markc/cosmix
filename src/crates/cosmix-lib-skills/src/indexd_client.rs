use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::SkillDocument;

const DEFAULT_SOCKET: &str = "/run/cosmix/embed.sock";

pub struct IndexdClient {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
}

impl IndexdClient {
    /// Connect using the socket path from cosmix settings.toml [embed] section.
    pub async fn from_config() -> Result<Self> {
        let cfg = cosmix_config::store::load().unwrap_or_default().embed;
        Self::connect(Some(&cfg.socket_path)).await
    }

    pub async fn connect(socket_path: Option<&str>) -> Result<Self> {
        let path = socket_path.unwrap_or(DEFAULT_SOCKET);
        let stream = UnixStream::connect(path)
            .await
            .with_context(|| format!("connecting to indexd at {path}"))?;
        let (reader, writer) = stream.into_split();
        Ok(Self {
            reader: BufReader::new(reader),
            writer,
        })
    }

    async fn request<T: for<'de> Deserialize<'de>>(&mut self, json: &str) -> Result<T> {
        self.writer.write_all(json.as_bytes()).await?;
        self.writer.write_all(b"\n").await?;
        self.writer.flush().await?;

        let mut line = String::new();
        self.reader.read_line(&mut line).await?;

        // Check for error response
        if let Ok(err) = serde_json::from_str::<ErrorResp>(&line) {
            if !err.error.is_empty() {
                anyhow::bail!("indexd error: {}", err.error);
            }
        }

        serde_json::from_str(&line).context("parsing indexd response")
    }

    /// Store a skill document in indexd. Returns the assigned ID.
    pub async fn store_skill(&mut self, skill: &SkillDocument) -> Result<i64> {
        let content = skill.to_markdown();
        let metadata = serde_json::to_string(skill)?;

        let req = serde_json::json!({
            "action": "store",
            "texts": [content],
            "source": "skill",
            "metadata": [metadata],
        });

        let resp: StoreResp = self.request(&req.to_string()).await?;
        resp.ids
            .into_iter()
            .next()
            .context("no id returned from store")
    }

    /// Search for skills relevant to a task description.
    /// If `domain` is Some, only returns skills from that domain.
    /// If None, returns skills from all domains.
    pub async fn search_skills(
        &mut self,
        task_description: &str,
        limit: usize,
    ) -> Result<Vec<(i64, SkillDocument, f64)>> {
        self.search_skills_domain(task_description, limit, None).await
    }

    /// Search with optional domain filtering.
    pub async fn search_skills_domain(
        &mut self,
        task_description: &str,
        limit: usize,
        domain: Option<&str>,
    ) -> Result<Vec<(i64, SkillDocument, f64)>> {
        let mut req = serde_json::json!({
            "action": "search",
            "query": task_description,
            "limit": limit,
            "source": "skill",
        });
        if let Some(d) = domain {
            req["metadata_filter"] = serde_json::json!([
                {"field": "domain", "op": "eq", "value": d}
            ]);
        }

        let resp: SearchResp = self.request(&req.to_string()).await?;
        let mut skills = Vec::new();
        for result in resp.results {
            if let Ok(doc) = serde_json::from_str::<SkillDocument>(&result.metadata) {
                skills.push((result.id, doc, result.distance));
            }
        }
        Ok(skills)
    }

    /// Update a skill's metadata and optionally re-embed with new content.
    pub async fn update_skill(&mut self, id: i64, skill: &SkillDocument) -> Result<()> {
        let content = skill.to_markdown();
        let metadata = serde_json::to_string(skill)?;

        let req = serde_json::json!({
            "action": "update",
            "id": id,
            "content": content,
            "metadata": metadata,
        });

        let _resp: UpdateResp = self.request(&req.to_string()).await?;
        Ok(())
    }

    /// List all skills, paginated.
    pub async fn list_skills(
        &mut self,
        limit: usize,
        offset: usize,
    ) -> Result<(Vec<(i64, SkillDocument)>, usize)> {
        let req = serde_json::json!({
            "action": "list",
            "source": "skill",
            "limit": limit,
            "offset": offset,
        });

        let resp: ListResp = self.request(&req.to_string()).await?;
        let mut skills = Vec::new();
        for item in resp.items {
            if let Ok(doc) = serde_json::from_str::<SkillDocument>(&item.metadata) {
                skills.push((item.id, doc));
            }
        }
        Ok((skills, resp.total))
    }

    /// Delete a skill by ID.
    pub async fn delete_skill(&mut self, id: i64) -> Result<()> {
        let req = serde_json::json!({
            "action": "delete",
            "ids": [id],
        });
        let _resp: DeleteResp = self.request(&req.to_string()).await?;
        Ok(())
    }

    /// Get database statistics from indexd.
    pub async fn stats(&mut self) -> Result<StatsResponse> {
        let req = serde_json::json!({"action": "stats"});
        self.request(&req.to_string()).await
    }

    /// Generic raw JSON request — send any valid indexd action and get the response.
    /// Use this for operations not covered by the typed methods (e.g. doc/journal indexing).
    pub async fn raw_request(&mut self, req: &serde_json::Value) -> Result<serde_json::Value> {
        self.request(&req.to_string()).await
    }

    /// Raw embed request (useful for testing).
    pub async fn embed(&mut self, texts: &[String], prefix: &str) -> Result<Vec<Vec<f32>>> {
        let req = serde_json::json!({
            "action": "embed",
            "texts": texts,
            "prefix": prefix,
        });
        let resp: EmbedResp = self.request(&req.to_string()).await?;
        Ok(resp.embeddings)
    }
}

// --- Response types matching indexd protocol ---

#[derive(Deserialize)]
struct ErrorResp {
    #[serde(default)]
    error: String,
}

#[derive(Deserialize)]
struct StoreResp {
    #[allow(dead_code)]
    stored: usize,
    #[serde(default)]
    #[allow(dead_code)]
    duplicates: usize,
    ids: Vec<i64>,
}

#[derive(Deserialize)]
struct SearchResultItem {
    id: i64,
    #[allow(dead_code)]
    content: String,
    #[allow(dead_code)]
    source: String,
    metadata: String,
    distance: f64,
}

#[derive(Deserialize)]
struct SearchResp {
    results: Vec<SearchResultItem>,
}

#[derive(Deserialize)]
struct UpdateResp {
    #[allow(dead_code)]
    updated: bool,
    #[allow(dead_code)]
    re_embedded: bool,
}

#[derive(Deserialize)]
struct ListItem {
    id: i64,
    #[allow(dead_code)]
    content: String,
    #[allow(dead_code)]
    source: String,
    metadata: String,
    #[allow(dead_code)]
    created: String,
}

#[derive(Deserialize)]
struct ListResp {
    items: Vec<ListItem>,
    total: usize,
}

#[derive(Deserialize)]
struct DeleteResp {
    #[allow(dead_code)]
    deleted: usize,
}

#[derive(Deserialize)]
struct EmbedResp {
    embeddings: Vec<Vec<f32>>,
}

/// Statistics from the indexd vector database.
#[derive(Debug, Deserialize)]
pub struct StatsResponse {
    pub total_vectors: usize,
    pub db_size_bytes: u64,
    pub model_loaded: bool,
    #[serde(default)]
    pub model_circuit: String,
    #[serde(default)]
    pub embed_cache_entries: usize,
    #[serde(default)]
    pub embed_cache_hits: u64,
    #[serde(default)]
    pub embed_cache_misses: u64,
    #[serde(default)]
    pub by_source: Vec<SourceCount>,
}

#[derive(Debug, Deserialize)]
pub struct SourceCount {
    pub source: String,
    pub count: usize,
}
