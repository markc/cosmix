use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use async_trait::async_trait;
use cosmix_llm::ToolSchema;
use serde_json::Value;

#[async_trait]
pub trait Tool: Send + Sync {
    fn schema(&self) -> ToolSchema;
    async fn run(&self, input: Value) -> Result<String>;
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        let name = tool.schema().name.clone();
        self.tools.insert(name, tool);
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|t| t.as_ref())
    }

    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools.values().map(|t| t.schema()).collect()
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

// ---- Built-in tools ----

/// Out-of-the-box file tools bundled for Phase-1 validation. Phase-2 will add
/// `run_mix` and Mix-script-based tool discovery.
pub struct BuiltinTools;

impl BuiltinTools {
    pub fn register_all(reg: &mut ToolRegistry) {
        reg.register(Box::new(ReadFile));
        reg.register(Box::new(WriteFile));
        reg.register(Box::new(ListFiles));
    }
}

struct ReadFile;

#[async_trait]
impl Tool for ReadFile {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "read_file".into(),
            description: "Read the full contents of a UTF-8 text file. Errors if the file is missing or not valid UTF-8.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file, absolute or relative to the agent's cwd."
                    }
                },
                "required": ["path"]
            }),
        }
    }

    async fn run(&self, input: Value) -> Result<String> {
        let path = input
            .get("path")
            .and_then(|v| v.as_str())
            .context("read_file: missing 'path'")?;
        let content = tokio::fs::read_to_string(PathBuf::from(path))
            .await
            .with_context(|| format!("reading {path}"))?;
        Ok(content)
    }
}

struct WriteFile;

#[async_trait]
impl Tool for WriteFile {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "write_file".into(),
            description: "Write a UTF-8 string to a file, overwriting any existing content. Creates parent directories as needed.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"]
            }),
        }
    }

    async fn run(&self, input: Value) -> Result<String> {
        let path = input
            .get("path")
            .and_then(|v| v.as_str())
            .context("write_file: missing 'path'")?;
        let content = input
            .get("content")
            .and_then(|v| v.as_str())
            .context("write_file: missing 'content'")?;
        let p = PathBuf::from(path);
        if let Some(parent) = p.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        tokio::fs::write(&p, content)
            .await
            .with_context(|| format!("writing {path}"))?;
        Ok(format!("wrote {} bytes to {path}", content.len()))
    }
}

struct ListFiles;

#[async_trait]
impl Tool for ListFiles {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "list_files".into(),
            description: "List the entries in a directory (non-recursive). Subdirectories are suffixed with '/'.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory path; defaults to '.' if omitted."
                    }
                }
            }),
        }
    }

    async fn run(&self, input: Value) -> Result<String> {
        let path = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let mut entries = tokio::fs::read_dir(path)
            .await
            .with_context(|| format!("listing {path}"))?;
        let mut out = Vec::new();
        while let Some(e) = entries.next_entry().await? {
            let name = e.file_name().to_string_lossy().to_string();
            let meta = e.metadata().await?;
            if meta.is_dir() {
                out.push(format!("{name}/"));
            } else {
                out.push(name);
            }
        }
        out.sort();
        Ok(out.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn registry_roundtrip() {
        let mut reg = ToolRegistry::new();
        BuiltinTools::register_all(&mut reg);
        assert_eq!(reg.len(), 3);
        assert!(reg.get("read_file").is_some());
        assert!(reg.get("write_file").is_some());
        assert!(reg.get("list_files").is_some());
        assert!(reg.get("nonexistent").is_none());
    }

    #[tokio::test]
    async fn read_write_roundtrip() {
        let tmp = std::env::temp_dir().join(format!("cosmix-agent-test-{}", uuid::Uuid::new_v4()));
        let path = tmp.join("hello.txt");

        let wf = WriteFile;
        let out = wf
            .run(serde_json::json!({
                "path": path.to_string_lossy(),
                "content": "hello world"
            }))
            .await
            .unwrap();
        assert!(out.contains("11 bytes"));

        let rf = ReadFile;
        let content = rf
            .run(serde_json::json!({"path": path.to_string_lossy()}))
            .await
            .unwrap();
        assert_eq!(content, "hello world");

        std::fs::remove_dir_all(&tmp).ok();
    }
}
