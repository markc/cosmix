//! JMAP core types (RFC 8620).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Top-level JMAP request (RFC 8620 §3.3).
#[derive(Debug, Deserialize)]
pub struct JmapRequest {
    pub using: Vec<String>,
    #[serde(rename = "methodCalls")]
    pub method_calls: Vec<MethodCall>,
}

/// A single method invocation: [name, arguments, call_id].
#[derive(Debug, Deserialize)]
pub struct MethodCall(pub String, pub serde_json::Value, pub String);

/// Top-level JMAP response (RFC 8620 §3.4).
#[derive(Debug, Serialize)]
pub struct JmapResponse {
    #[serde(rename = "methodResponses")]
    pub method_responses: Vec<MethodResponse>,
    #[serde(rename = "sessionState")]
    pub session_state: String,
}

/// A single method response: [name, response, call_id].
#[derive(Debug, Serialize)]
pub struct MethodResponse(pub String, pub serde_json::Value, pub String);

/// JMAP Session resource (RFC 8620 §2).
#[derive(Debug, Serialize)]
pub struct Session {
    pub capabilities: HashMap<String, serde_json::Value>,
    pub accounts: HashMap<String, AccountInfo>,
    #[serde(rename = "primaryAccounts")]
    pub primary_accounts: HashMap<String, String>,
    pub username: String,
    #[serde(rename = "apiUrl")]
    pub api_url: String,
    #[serde(rename = "downloadUrl")]
    pub download_url: String,
    #[serde(rename = "uploadUrl")]
    pub upload_url: String,
    #[serde(rename = "eventSourceUrl")]
    pub event_source_url: String,
    pub state: String,
}

#[derive(Debug, Serialize)]
pub struct AccountInfo {
    pub name: String,
    #[serde(rename = "isPersonal")]
    pub is_personal: bool,
    #[serde(rename = "isReadOnly")]
    pub is_read_only: bool,
    #[serde(rename = "accountCapabilities")]
    pub account_capabilities: HashMap<String, serde_json::Value>,
}

/// Standard JMAP /get response.
#[derive(Debug, Serialize)]
pub struct GetResponse<T: Serialize> {
    #[serde(rename = "accountId")]
    pub account_id: String,
    pub state: String,
    pub list: Vec<T>,
    #[serde(rename = "notFound")]
    pub not_found: Vec<String>,
}

/// Standard JMAP /set response.
#[derive(Debug, Serialize)]
pub struct SetResponse {
    #[serde(rename = "accountId")]
    pub account_id: String,
    #[serde(rename = "oldState")]
    pub old_state: String,
    #[serde(rename = "newState")]
    pub new_state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created: Option<HashMap<String, serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated: Option<HashMap<String, serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destroyed: Option<Vec<String>>,
    #[serde(rename = "notCreated", skip_serializing_if = "Option::is_none")]
    pub not_created: Option<HashMap<String, SetError>>,
    #[serde(rename = "notUpdated", skip_serializing_if = "Option::is_none")]
    pub not_updated: Option<HashMap<String, SetError>>,
    #[serde(rename = "notDestroyed", skip_serializing_if = "Option::is_none")]
    pub not_destroyed: Option<HashMap<String, SetError>>,
}

#[derive(Debug, Serialize)]
pub struct SetError {
    #[serde(rename = "type")]
    pub error_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Standard JMAP /query response.
#[derive(Debug, Serialize)]
pub struct QueryResponse {
    #[serde(rename = "accountId")]
    pub account_id: String,
    #[serde(rename = "queryState")]
    pub query_state: String,
    #[serde(rename = "canCalculateChanges")]
    pub can_calculate_changes: bool,
    pub position: u64,
    pub ids: Vec<String>,
    pub total: u64,
}

/// Standard JMAP /changes response.
#[derive(Debug, Serialize)]
pub struct ChangesResponse {
    #[serde(rename = "accountId")]
    pub account_id: String,
    #[serde(rename = "oldState")]
    pub old_state: String,
    #[serde(rename = "newState")]
    pub new_state: String,
    #[serde(rename = "hasMoreChanges")]
    pub has_more_changes: bool,
    pub created: Vec<String>,
    pub updated: Vec<String>,
    pub destroyed: Vec<String>,
}

/// JMAP error response.
#[derive(Debug, Serialize)]
pub struct JmapError {
    #[serde(rename = "type")]
    pub error_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl JmapError {
    pub fn method_not_found(method: &str) -> Self {
        Self {
            error_type: "unknownMethod".into(),
            description: Some(format!("Unknown method: {method}")),
        }
    }

    pub fn invalid_arguments(desc: &str) -> Self {
        Self {
            error_type: "invalidArguments".into(),
            description: Some(desc.into()),
        }
    }

    pub fn not_found() -> Self {
        Self {
            error_type: "notFound".into(),
            description: None,
        }
    }
}

/// JMAP capabilities URN constants.
pub const CAPABILITY_CORE: &str = "urn:ietf:params:jmap:core";
pub const CAPABILITY_MAIL: &str = "urn:ietf:params:jmap:mail";
pub const CAPABILITY_SUBMISSION: &str = "urn:ietf:params:jmap:submission";
pub const CAPABILITY_CALENDARS: &str = "urn:ietf:params:jmap:calendars";
pub const CAPABILITY_CONTACTS: &str = "urn:ietf:params:jmap:contacts";
