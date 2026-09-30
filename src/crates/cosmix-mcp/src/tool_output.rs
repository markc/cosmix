//! Wire contracts shared by the legacy text tools. Typed/native handlers keep
//! their own schemas. Human-readable content remains alongside typed results.

use rmcp::model::{
    CallToolResponse, CallToolResult, ContentBlock as Content, Tool, ToolAnnotations,
};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Serialize, JsonSchema)]
pub(crate) struct TextObservation {
    pub result: Value,
}

#[derive(Serialize, JsonSchema)]
pub(crate) struct BusObservation {
    pub result: Value,
    pub rc: u8,
}

#[derive(Serialize, JsonSchema)]
pub(crate) struct MixExecutionObservation {
    pub result: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

#[derive(Serialize, JsonSchema)]
pub(crate) struct StatusSnapshot {
    pub started: String,
    pub uptime_secs: u64,
    pub broker_connected: bool,
    pub total_calls: u64,
    pub error_calls: u64,
    pub per_tool: std::collections::BTreeMap<String, u64>,
    pub recent: Vec<RecentCall>,
}

#[derive(Serialize, JsonSchema)]
pub(crate) struct RecentCall {
    pub tool: String,
    pub at: String,
    pub ms: u64,
    pub ok: bool,
}

#[derive(Serialize, JsonSchema)]
pub(crate) struct TermListObservation {
    pub service: String,
    pub instance: Option<u64>,
    /// Parsed native tab rows, including each row's instance metadata.
    pub tabs: Vec<Value>,
    /// Parsed native pane rows, including each row's instance metadata.
    pub panes: Vec<Value>,
}

pub(crate) fn decorate(mut tool: Tool) -> Tool {
    if tool.output_schema.is_none() {
        tool.output_schema = Some(if tool.name == "bus_call" {
            rmcp::handler::server::common::schema_for_output::<BusObservation>()
        } else if tool.name == "mix_execute" {
            rmcp::handler::server::common::schema_for_output::<MixExecutionObservation>()
        } else {
            rmcp::handler::server::common::schema_for_output::<TextObservation>()
        });
    }
    if tool.annotations.is_none() {
        let read_only = matches!(
            tool.name.as_ref(),
            "term_list"
                | "term_snapshot"
                | "bus_list_services"
                | "bus_node_info"
                | "bus_list_peers"
                | "noded_ping"
                | "log_tail"
                | "log_search"
                | "skills_list"
                | "mcp_status"
        );
        // Retrieval/search can update scoring or consult external LLM services;
        // unknown tools stay conservative rather than claiming idempotence.
        tool.annotations = Some(
            ToolAnnotations::new()
                .read_only(read_only)
                .destructive(!read_only)
                .idempotent(read_only)
                .open_world(true),
        );
    }
    tool
}

pub(crate) fn normalize(tool: &str, response: CallToolResponse) -> CallToolResponse {
    let CallToolResponse::Complete(mut result) = response else {
        return response;
    };
    let texts = result
        .content
        .iter()
        .filter_map(|block| block.as_text().map(|text| text.text.as_str()))
        .collect::<Vec<_>>();
    // Compatibility boundary for legacy handlers. Do not scan arbitrary log or
    // screen lines for ERROR:; an observed line is data, not a tool failure.
    let legacy_status = matches!(
        tool,
        "bus_list_services"
            | "bus_node_info"
            | "bus_list_peers"
            | "noded_ping"
            | "context_search"
            | "index_workspace"
            | "skills_retrieve"
            | "skills_store"
            | "skills_refine"
            | "skills_list"
            | "skills_delete"
            | "skills_graduate"
            | "docs_feedback"
            | "journal_feedback"
            | "memory_feedback"
            | "journal_supersede"
            | "knowledge_digest"
            | "knowledge_brief"
    );
    if legacy_status
        && texts.len() == 1
        && (texts[0].starts_with("ERROR:") || texts[0].starts_with("ERROR "))
    {
        result.is_error = Some(true);
    }
    if !result.is_error.unwrap_or(false) && result.structured_content.is_none() {
        let text = texts.join("\n");
        let value = serde_json::from_str(&text).unwrap_or(Value::String(text));
        result.structured_content = Some(json!({"result": value}));
    }
    CallToolResponse::Complete(result)
}

pub(crate) fn failure(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![Content::text(message.into())])
}

pub(crate) fn bus_reply(reply: cosmix_bus::PortReply) -> CallToolResult {
    match reply {
        cosmix_bus::PortReply::Ok { rc, value } => {
            CallToolResult::structured(json!({"result": value, "rc": rc}))
        }
        cosmix_bus::PortReply::AppError { rc, message } => {
            let mut result = CallToolResult::structured(json!({"result": {
                "outcome": "application_error", "rc": rc, "message": message
            }, "rc": rc}));
            result.is_error = Some(true);
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_text_is_not_success_and_observed_error_lines_are_data() {
        let CallToolResponse::Complete(error) = normalize(
            "bus_list_services",
            CallToolResult::success(vec![Content::text("ERROR: refused")]).into(),
        ) else {
            panic!()
        };
        assert_eq!(error.is_error, Some(true));
        let CallToolResponse::Complete(observed) = normalize(
            "log_tail",
            CallToolResult::success(vec![Content::text("ERROR: previous command")]).into(),
        ) else {
            panic!()
        };
        assert!(!observed.is_error.unwrap_or(false));
        assert!(observed.structured_content.is_some());
    }

    #[test]
    fn annotations_are_conservative_for_arbitrary_mutations() {
        for name in ["bus_call", "mix_execute", "term_type", "new_tool"] {
            let mut tool = Tool::default();
            tool.name = name.into();
            let annotations = decorate(tool).annotations.unwrap();
            assert_eq!(annotations.read_only_hint, Some(false));
            assert_eq!(annotations.idempotent_hint, Some(false));
        }
    }
}
