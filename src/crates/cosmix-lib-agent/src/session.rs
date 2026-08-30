use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use cosmix_config::{CosmixDir, cosmix_path};
use cosmix_llm::{Message, Usage};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub type SessionId = String;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub created_at: DateTime<Utc>,
    pub system: Option<String>,
    pub backend: Option<String>,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub total_usage: Usage,
}

impl Session {
    pub fn new(system: Option<String>, backend: Option<String>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            created_at: Utc::now(),
            system,
            backend,
            messages: Vec::new(),
            total_usage: Usage::default(),
        }
    }

    pub fn append(&mut self, msg: Message) {
        self.messages.push(msg);
    }

    pub fn add_usage(&mut self, u: Usage) {
        self.total_usage.input_tokens =
            self.total_usage.input_tokens.saturating_add(u.input_tokens);
        self.total_usage.output_tokens = self
            .total_usage
            .output_tokens
            .saturating_add(u.output_tokens);
    }

    pub fn jsonl_path(&self) -> PathBuf {
        sessions_dir().join(format!("{}.jsonl", self.id))
    }

    /// Persist the session as JSONL: first line is a header record, each
    /// subsequent line is one message. MVP writes the whole file atomically
    /// via tmp+rename; Phase-2 can switch to append-only.
    pub fn save(&self) -> Result<()> {
        let dir = sessions_dir();
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = self.jsonl_path();
        let tmp = path.with_extension("jsonl.tmp");

        let mut out = String::new();
        let header = serde_json::json!({
            "type": "header",
            "id": self.id,
            "created_at": self.created_at,
            "system": self.system,
            "backend": self.backend,
            "total_usage": self.total_usage,
        });
        out.push_str(&serde_json::to_string(&header)?);
        out.push('\n');
        for m in &self.messages {
            let rec = serde_json::json!({
                "type": "message",
                "message": m,
            });
            out.push_str(&serde_json::to_string(&rec)?);
            out.push('\n');
        }
        std::fs::write(&tmp, out)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn load(id: &str) -> Result<Self> {
        let path = sessions_dir().join(format!("{id}.jsonl"));
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let mut lines = text.lines();

        let header_line = lines.next().context("empty session file")?;
        let header: serde_json::Value = serde_json::from_str(header_line)?;

        let id = header
            .get("id")
            .and_then(|v| v.as_str())
            .context("header missing id")?
            .to_string();
        let created_at: DateTime<Utc> = serde_json::from_value(
            header
                .get("created_at")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        )
        .unwrap_or_else(|_| Utc::now());
        let system = header
            .get("system")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let backend = header
            .get("backend")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let total_usage: Usage = serde_json::from_value(
            header
                .get("total_usage")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        )
        .unwrap_or_default();

        let mut messages = Vec::new();
        for line in lines {
            if line.trim().is_empty() {
                continue;
            }
            let rec: serde_json::Value = serde_json::from_str(line)?;
            if let Some(m) = rec.get("message") {
                let msg: Message = serde_json::from_value(m.clone())?;
                messages.push(msg);
            }
        }

        Ok(Self {
            id,
            created_at,
            system,
            backend,
            messages,
            total_usage,
        })
    }

    pub fn list_ids() -> Vec<String> {
        let dir = sessions_dir();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut ids: Vec<String> = entries
            .filter_map(|e| {
                let e = e.ok()?;
                let name = e.file_name().into_string().ok()?;
                name.strip_suffix(".jsonl").map(|s| s.to_string())
            })
            .collect();
        ids.sort();
        ids
    }

    pub fn delete(id: &str) -> Result<()> {
        let path = sessions_dir().join(format!("{id}.jsonl"));
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        Ok(())
    }
}

pub fn sessions_dir() -> PathBuf {
    cosmix_path(CosmixDir::Var).join("agent").join("sessions")
}
