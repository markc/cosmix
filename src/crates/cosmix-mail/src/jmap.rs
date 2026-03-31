use anyhow::{anyhow, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};

// --- Wire types ---

#[derive(Serialize)]
pub struct JmapRequest {
    pub using: Vec<String>,
    #[serde(rename = "methodCalls")]
    pub method_calls: Vec<(String, serde_json::Value, String)>,
}

#[derive(Deserialize, Debug)]
pub struct JmapResponse {
    #[serde(rename = "methodResponses")]
    pub method_responses: Vec<(String, serde_json::Value, String)>,
}

// --- Data types ---

#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct Mailbox {
    pub id: String,
    pub name: String,
    #[serde(rename = "parentId")]
    pub parent_id: Option<String>,
    pub role: Option<String>,
    #[serde(rename = "sortOrder")]
    pub sort_order: i32,
    #[serde(rename = "totalEmails", default)]
    pub total_emails: u32,
    #[serde(rename = "unreadEmails", default)]
    pub unread_emails: u32,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct EmailAddress {
    pub name: Option<String>,
    pub email: String,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct Email {
    pub id: String,
    pub subject: Option<String>,
    pub from: Option<Vec<EmailAddress>>,
    pub to: Option<Vec<EmailAddress>>,
    pub cc: Option<Vec<EmailAddress>>,
    #[serde(rename = "messageId")]
    pub message_id: Option<Vec<String>>,
    pub date: Option<String>,
    pub preview: Option<String>,
    #[serde(rename = "receivedAt")]
    pub received_at: Option<String>,
    #[serde(rename = "hasAttachment")]
    pub has_attachment: Option<bool>,
    pub keywords: Option<serde_json::Value>,
    #[serde(rename = "bodyValues")]
    pub body_values: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(rename = "textBody")]
    pub text_body: Option<Vec<BodyPart>>,
    #[serde(rename = "htmlBody")]
    pub html_body: Option<Vec<BodyPart>>,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct BodyPart {
    #[serde(rename = "partId")]
    pub part_id: String,
    #[serde(rename = "type")]
    pub content_type: Option<String>,
}

impl Email {
    pub fn text_body_value(&self) -> Option<&str> {
        let body_values = self.body_values.as_ref()?;
        let text_parts = self.text_body.as_ref()?;
        let part = text_parts.first()?;
        let val = body_values.get(&part.part_id)?;
        val.get("value")?.as_str()
    }

    pub fn html_body_value(&self) -> Option<&str> {
        let body_values = self.body_values.as_ref()?;
        let html_parts = self.html_body.as_ref()?;
        let part = html_parts.first()?;
        let val = body_values.get(&part.part_id)?;
        val.get("value")?.as_str()
    }

    pub fn from_display(&self) -> String {
        self.from
            .as_ref()
            .and_then(|addrs| addrs.first())
            .map(|a| {
                if let Some(name) = &a.name {
                    if !name.is_empty() {
                        return name.clone();
                    }
                }
                a.email.clone()
            })
            .unwrap_or_default()
    }

    pub fn date_short(&self) -> String {
        self.date
            .as_deref()
            .or(self.received_at.as_deref())
            .and_then(|d| {
                chrono::DateTime::parse_from_rfc3339(d)
                    .ok()
                    .map(|dt| dt.format("%b %d %H:%M").to_string())
            })
            .unwrap_or_default()
    }

    pub fn is_read(&self) -> bool {
        self.keywords
            .as_ref()
            .and_then(|k| k.get("$seen"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }
}

// --- GetResponse wrapper ---

#[derive(Deserialize, Debug)]
pub struct GetResponse<T> {
    #[serde(rename = "accountId")]
    #[allow(dead_code)]
    pub account_id: String,
    #[allow(dead_code)]
    pub state: String,
    pub list: Vec<T>,
}

#[derive(Deserialize, Debug)]
pub struct QueryResponse {
    pub ids: Vec<String>,
    #[allow(dead_code)]
    pub total: Option<u64>,
}

#[derive(Deserialize, Debug)]
pub struct BlobUploadResponse {
    #[serde(rename = "blobId")]
    pub blob_id: String,
    #[allow(dead_code)]
    pub size: u64,
}

// --- Client ---

#[derive(Clone)]
pub struct JmapClient {
    http: Client,
    base_url: String,
    email: String,
    password: String,
}

impl JmapClient {
    pub fn new(base_url: &str, email: &str, password: &str) -> Result<Self> {
        #[cfg(not(target_arch = "wasm32"))]
        let http = Client::builder()
            .danger_accept_invalid_certs(true)
            .build()?;
        #[cfg(target_arch = "wasm32")]
        let http = Client::new();
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            email: email.to_string(),
            password: password.to_string(),
        })
    }

    async fn call(&self, method_calls: Vec<(String, serde_json::Value, String)>) -> Result<JmapResponse> {
        let req = JmapRequest {
            using: vec![
                "urn:ietf:params:jmap:core".into(),
                "urn:ietf:params:jmap:mail".into(),
                "urn:ietf:params:jmap:submission".into(),
            ],
            method_calls,
        };
        let resp = self
            .http
            .post(format!("{}/jmap", self.base_url))
            .basic_auth(&self.email, Some(&self.password))
            .json(&req)
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(anyhow!("JMAP request failed: {}", resp.status()));
        }
        Ok(resp.json().await?)
    }

    pub async fn mailboxes(&self) -> Result<Vec<Mailbox>> {
        let resp = self
            .call(vec![("Mailbox/get".into(), serde_json::json!({}), "m0".into())])
            .await?;
        let (_, data, _) = resp
            .method_responses
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("empty response"))?;
        let get: GetResponse<Mailbox> = serde_json::from_value(data)?;
        Ok(get.list)
    }

    pub async fn email_ids(&self, mailbox_id: &str) -> Result<Vec<String>> {
        let resp = self
            .call(vec![(
                "Email/query".into(),
                serde_json::json!({
                    "filter": { "inMailbox": mailbox_id },
                    "sort": [{ "property": "receivedAt", "isAscending": false }],
                    "limit": 50
                }),
                "q0".into(),
            )])
            .await?;
        let (_, data, _) = resp
            .method_responses
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("empty response"))?;
        let qr: QueryResponse = serde_json::from_value(data)?;
        Ok(qr.ids)
    }

    pub async fn emails(&self, ids: &[String], fetch_body: bool) -> Result<Vec<Email>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let mut args = serde_json::json!({ "ids": ids });
        if fetch_body {
            args["fetchTextBodyValues"] = serde_json::json!(true);
            args["fetchHTMLBodyValues"] = serde_json::json!(true);
        }
        let resp = self
            .call(vec![("Email/get".into(), args, "e0".into())])
            .await?;
        let (_, data, _) = resp
            .method_responses
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("empty response"))?;
        let get: GetResponse<Email> = serde_json::from_value(data)?;
        Ok(get.list)
    }

    /// Update email properties (keywords, mailboxIds).
    pub async fn update_email(&self, id: &str, patch: serde_json::Value) -> Result<()> {
        let resp = self
            .call(vec![(
                "Email/set".into(),
                serde_json::json!({ "update": { id: patch } }),
                "u0".into(),
            )])
            .await?;
        let (_, data, _) = resp
            .method_responses
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("empty response"))?;
        if let Some(errors) = data.get("notUpdated").and_then(|v| v.as_object()) {
            if let Some(err) = errors.get(id) {
                return Err(anyhow!("Update failed: {}", err));
            }
        }
        Ok(())
    }

    /// Permanently destroy an email.
    pub async fn destroy_email(&self, id: &str) -> Result<()> {
        let resp = self
            .call(vec![(
                "Email/set".into(),
                serde_json::json!({ "destroy": [id] }),
                "d0".into(),
            )])
            .await?;
        let (_, data, _) = resp
            .method_responses
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("empty response"))?;
        if let Some(errors) = data.get("notDestroyed").and_then(|v| v.as_object()) {
            if let Some(err) = errors.get(id) {
                return Err(anyhow!("Destroy failed: {}", err));
            }
        }
        Ok(())
    }

    /// Upload raw bytes as a blob, returns the blobId.
    pub async fn upload_blob(&self, data: &[u8]) -> Result<String> {
        let resp = self
            .http
            .post(format!("{}/jmap/upload/1", self.base_url))
            .basic_auth(&self.email, Some(&self.password))
            .header("Content-Type", "application/octet-stream")
            .body(data.to_vec())
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(anyhow!("Blob upload failed: {}", resp.status()));
        }
        let upload: BlobUploadResponse = resp.json().await?;
        Ok(upload.blob_id)
    }

    /// Create an email record from an uploaded blob.
    pub async fn create_email(
        &self,
        blob_id: &str,
        mailbox_id: &str,
        keywords: serde_json::Value,
    ) -> Result<String> {
        let resp = self
            .call(vec![(
                "Email/set".into(),
                serde_json::json!({
                    "create": {
                        "draft1": {
                            "blobId": blob_id,
                            "mailboxIds": { mailbox_id: true },
                            "keywords": keywords
                        }
                    }
                }),
                "c0".into(),
            )])
            .await?;
        let (_, data, _) = resp
            .method_responses
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("empty response"))?;
        if let Some(errors) = data.get("notCreated").and_then(|v| v.as_object()) {
            if let Some(err) = errors.get("draft1") {
                return Err(anyhow!("Create failed: {}", err));
            }
        }
        let email_id = data
            .get("created")
            .and_then(|c| c.get("draft1"))
            .and_then(|d| d.get("id"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("No id in create response"))?;
        Ok(email_id.to_string())
    }

    /// Submit an email for SMTP delivery.
    pub async fn submit_email(&self, email_id: &str) -> Result<()> {
        let resp = self
            .call(vec![(
                "EmailSubmission/set".into(),
                serde_json::json!({
                    "create": {
                        "sub1": {
                            "emailId": email_id,
                            "identityId": "1"
                        }
                    }
                }),
                "s0".into(),
            )])
            .await?;
        let (_, data, _) = resp
            .method_responses
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("empty response"))?;
        if let Some(errors) = data.get("notCreated").and_then(|v| v.as_object()) {
            if let Some(err) = errors.get("sub1") {
                return Err(anyhow!("Submit failed: {}", err));
            }
        }
        Ok(())
    }

    /// High-level compose + send: build MIME → upload blob → create email → submit.
    pub async fn send_compose(
        &self,
        from: &str,
        to: &[String],
        cc: &[String],
        subject: &str,
        body: &str,
        in_reply_to: Option<&str>,
        drafts_mailbox_id: &str,
    ) -> Result<()> {
        let mime_bytes = build_mime(from, to, cc, subject, body, in_reply_to);

        // Upload blob
        let blob_id = self.upload_blob(&mime_bytes).await?;

        // Create email record (as draft, seen)
        let email_id = self
            .create_email(
                &blob_id,
                drafts_mailbox_id,
                serde_json::json!({"$seen": true, "$draft": true}),
            )
            .await?;

        // Submit for delivery (server moves to Sent automatically)
        self.submit_email(&email_id).await?;

        Ok(())
    }
}

/// Build a minimal RFC 5322 plain text message without external crates.
fn build_mime(
    from: &str,
    to: &[String],
    cc: &[String],
    subject: &str,
    body: &str,
    in_reply_to: Option<&str>,
) -> Vec<u8> {
    let message_id = format!("<{}.{}@cosmix>", uuid::Uuid::new_v4(), chrono::Utc::now().timestamp());
    let date = chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S +0000").to_string();

    let mut mime = String::new();
    mime.push_str(&format!("From: {from}\r\n"));
    mime.push_str(&format!("To: {}\r\n", to.join(", ")));
    if !cc.is_empty() {
        mime.push_str(&format!("Cc: {}\r\n", cc.join(", ")));
    }
    mime.push_str(&format!("Subject: {subject}\r\n"));
    mime.push_str(&format!("Date: {date}\r\n"));
    mime.push_str(&format!("Message-ID: {message_id}\r\n"));
    if let Some(reply_id) = in_reply_to {
        mime.push_str(&format!("In-Reply-To: <{reply_id}>\r\n"));
        mime.push_str(&format!("References: <{reply_id}>\r\n"));
    }
    mime.push_str("MIME-Version: 1.0\r\n");
    mime.push_str("Content-Type: text/plain; charset=utf-8\r\n");
    mime.push_str("Content-Transfer-Encoding: 8bit\r\n");
    mime.push_str("\r\n");
    mime.push_str(body);

    mime.into_bytes()
}
