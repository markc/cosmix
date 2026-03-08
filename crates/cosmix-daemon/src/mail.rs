use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::sync::{LazyLock, Mutex};

// ---------------------------------------------------------------------------
// Reusable JmapClient — no global state, suitable for multi-account use
// ---------------------------------------------------------------------------

pub fn base64_encode(input: &str) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = input.as_bytes();
    let mut result = String::new();
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        result.push(CHARS[((n >> 18) & 63) as usize] as char);
        result.push(CHARS[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            result.push(CHARS[((n >> 6) & 63) as usize] as char);
        } else {
            result.push('=');
        }
        if chunk.len() > 2 {
            result.push(CHARS[(n & 63) as usize] as char);
        } else {
            result.push('=');
        }
    }
    result
}

fn new_agent() -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::config::Config::builder().https_only(false).build(),
    )
}

/// A reusable JMAP client for a single account.
/// No global state — create one per account, store wherever you like.
#[derive(Clone)]
pub struct JmapClient {
    pub api_url: String,
    pub account_id: String,
    pub auth_header: String,
    pub name: String,
    pub user: String,
}

impl JmapClient {
    /// Connect to a JMAP server, performing session discovery.
    /// Tries {url}/jmap/session (Stalwart) then {url}/.well-known/jmap (standard).
    pub fn connect(url: &str, user: &str, pass: &str) -> Result<Self> {
        Self::connect_named("", url, user, pass)
    }

    /// Connect with a display name for the account.
    pub fn connect_named(name: &str, url: &str, user: &str, pass: &str) -> Result<Self> {
        let auth_header = format!("Basic {}", base64_encode(&format!("{user}:{pass}")));
        let agent = new_agent();

        let base = url.trim_end_matches('/');
        let try_urls = [
            format!("{base}/jmap/session"),
            format!("{base}/.well-known/jmap"),
        ];

        let mut session_resp: Option<Value> = None;
        let mut last_err = String::new();
        for try_url in &try_urls {
            match agent.get(try_url)
                .header("Authorization", &auth_header)
                .call()
            {
                Ok(mut resp) => {
                    if let Ok(json) = resp.body_mut().read_json::<Value>() {
                        if json.get("apiUrl").is_some() {
                            session_resp = Some(json);
                            break;
                        }
                    }
                }
                Err(e) => { last_err = e.to_string(); }
            }
        }

        let session_resp = session_resp
            .context(format!("JMAP session discovery failed on {base}: {last_err}"))?;

        let api_url_raw = session_resp["apiUrl"].as_str()
            .context("No apiUrl in JMAP session")?;

        let api_url = if api_url_raw.starts_with("http") {
            api_url_raw.to_string()
        } else {
            format!("{base}{api_url_raw}")
        };

        let account_id = session_resp["primaryAccounts"]["urn:ietf:params:jmap:mail"]
            .as_str()
            .context("No primary mail account")?
            .to_string();

        Ok(Self {
            api_url,
            account_id,
            auth_header,
            name: name.to_string(),
            user: user.to_string(),
        })
    }

    /// Make a raw JMAP API call with the given method calls.
    pub fn call(&self, method_calls: Vec<Value>) -> Result<Value> {
        let body = json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            "methodCalls": method_calls,
        });

        let agent = new_agent();
        let resp: Value = agent.post(&self.api_url)
            .header("Authorization", &self.auth_header)
            .header("Content-Type", "application/json")
            .send_json(&body)?
            .body_mut()
            .read_json()?;
        Ok(resp)
    }

    pub fn mailboxes(&self) -> Result<Value> {
        let resp = self.call(vec![json!([
            "Mailbox/get",
            { "accountId": self.account_id, "properties": ["name", "totalEmails", "unreadEmails", "role"] },
            "0"
        ])])?;
        let list = &resp["methodResponses"][0][1]["list"];
        Ok(list.clone())
    }

    pub fn query(&self, mailbox_id: Option<&str>, limit: Option<u32>) -> Result<Value> {
        let limit = limit.unwrap_or(10);
        let filter = match mailbox_id {
            Some(mb) => json!({ "inMailbox": mb }),
            None => json!({}),
        };

        let resp = self.call(vec![
            json!(["Email/query", {
                "accountId": self.account_id,
                "filter": filter,
                "sort": [{ "property": "receivedAt", "isAscending": false }],
                "limit": limit,
            }, "0"]),
            json!(["Email/get", {
                "accountId": self.account_id,
                "#ids": { "resultOf": "0", "name": "Email/query", "path": "/ids" },
                "properties": ["id", "from", "subject", "receivedAt", "preview"],
            }, "1"]),
        ])?;

        let emails = &resp["methodResponses"][1][1]["list"];
        Ok(emails.clone())
    }

    pub fn read(&self, id: &str) -> Result<Value> {
        let resp = self.call(vec![json!(["Email/get", {
            "accountId": self.account_id,
            "ids": [id],
            "properties": ["id", "from", "to", "subject", "receivedAt", "textBody", "bodyValues"],
            "fetchTextBodyValues": true,
        }, "0"])])?;

        let email = &resp["methodResponses"][0][1]["list"][0];
        Ok(email.clone())
    }

    pub fn send(&self, to: &str, subject: &str, body: &str) -> Result<Value> {
        let resp = self.call(vec![
            json!(["Email/set", {
                "accountId": self.account_id,
                "create": {
                    "draft": {
                        "to": [{ "email": to }],
                        "subject": subject,
                        "textBody": [{ "partId": "1", "type": "text/plain" }],
                        "bodyValues": { "1": { "value": body } },
                        "keywords": { "$draft": true },
                    }
                }
            }, "0"]),
            json!(["EmailSubmission/set", {
                "accountId": self.account_id,
                "create": {
                    "sub": {
                        "#emailId": { "resultOf": "0", "name": "Email/set", "path": "/created/draft/id" },
                    }
                }
            }, "1"]),
        ])?;
        Ok(resp)
    }

    pub fn reply(&self, id: &str, body_text: &str) -> Result<Value> {
        let original = self.read(id)?;
        let from = &original["from"][0]["email"];
        let subject = original["subject"].as_str().unwrap_or("");
        let reply_subject = if subject.starts_with("Re: ") {
            subject.to_string()
        } else {
            format!("Re: {subject}")
        };

        let resp = self.call(vec![
            json!(["Email/set", {
                "accountId": self.account_id,
                "create": {
                    "draft": {
                        "to": [{ "email": from }],
                        "subject": reply_subject,
                        "inReplyTo": [id],
                        "textBody": [{ "partId": "1", "type": "text/plain" }],
                        "bodyValues": { "1": { "value": body_text } },
                    }
                }
            }, "0"]),
            json!(["EmailSubmission/set", {
                "accountId": self.account_id,
                "create": {
                    "sub": {
                        "#emailId": { "resultOf": "0", "name": "Email/set", "path": "/created/draft/id" },
                    }
                }
            }, "1"]),
        ])?;
        Ok(resp)
    }

    pub fn delete(&self, id: &str) -> Result<Value> {
        let resp = self.call(vec![json!(["Email/set", {
            "accountId": self.account_id,
            "destroy": [id],
        }, "0"])])?;
        Ok(resp)
    }
}

// ---------------------------------------------------------------------------
// Global convenience wrappers for Lua scripting (single-account, backward compat)
// ---------------------------------------------------------------------------

static SESSION: LazyLock<Mutex<Option<JmapClient>>> = LazyLock::new(|| Mutex::new(None));

fn with_client<F, T>(f: F) -> Result<T>
where F: FnOnce(&JmapClient) -> Result<T> {
    let guard = SESSION.lock().unwrap();
    let client = guard.as_ref().context("Not connected — call cosmix.mail.connect() first")?;
    f(client)
}

pub fn connect(url: &str, user: &str, pass: &str) -> Result<()> {
    let client = JmapClient::connect(url, user, pass)?;
    let mut s = SESSION.lock().unwrap();
    *s = Some(client);
    Ok(())
}

pub fn auto_connect() -> Result<()> {
    let url = std::env::var("JMAP_URL").context("JMAP_URL not set")?;
    let user = std::env::var("JMAP_USER").context("JMAP_USER not set")?;
    let pass = std::env::var("JMAP_PASS").context("JMAP_PASS not set")?;
    connect(&url, &user, &pass)
}

pub fn ensure_connected() -> Result<()> {
    let s = SESSION.lock().unwrap();
    if s.is_some() { return Ok(()); }
    drop(s);
    auto_connect()
}

pub fn mailboxes() -> Result<Value> {
    ensure_connected()?;
    with_client(|c| c.mailboxes())
}

pub fn query(mailbox_id: Option<&str>, limit: Option<u32>) -> Result<Value> {
    ensure_connected()?;
    with_client(|c| c.query(mailbox_id, limit))
}

pub fn read(id: &str) -> Result<Value> {
    ensure_connected()?;
    with_client(|c| c.read(id))
}

pub fn send(to: &str, subject: &str, body: &str) -> Result<Value> {
    ensure_connected()?;
    with_client(|c| c.send(to, subject, body))
}

pub fn reply(id: &str, body_text: &str) -> Result<Value> {
    ensure_connected()?;
    with_client(|c| c.reply(id, body_text))
}
