//! Typed native administrative submission through the existing mail pipeline.
//! A durable reservation prevents an uncertain response from duplicating mail.

use std::sync::Arc;

use anyhow::{Result, ensure};
use cosmix_client::IncomingCommand;
use cosmix_mds::{Flags, Mds, Tags};
use cosmix_props::runtime::Runtime;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::db::Db;
use crate::mailstore::{MailStore, MailboxRole, SqliteMailStore};

const MAX_TEXT: usize = 256 * 1024;
const MAX_REQUEST: usize = MAX_TEXT * 6 + 16 * 1024;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Request {
    operation_id: String,
    account: String,
    #[serde(default)]
    from: Option<String>,
    to: Vec<String>,
    subject: String,
    text: String,
}

impl Request {
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.operation_id.is_empty()
                && self.operation_id.len() <= 128
                && self
                    .operation_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)),
            "invalid operation_id"
        );
        address(&self.account)?;
        if let Some(from) = &self.from {
            address(from)?;
        }
        ensure!(
            !self.to.is_empty() && self.to.len() <= 32,
            "recipient count must be 1..32"
        );
        let mut recipients = std::collections::BTreeSet::new();
        for recipient in &self.to {
            address(recipient)?;
            ensure!(
                recipients.insert(recipient.to_ascii_lowercase()),
                "duplicate recipient"
            );
        }
        ensure!(
            self.subject.len() <= 4096
                && !self
                    .subject
                    .chars()
                    .any(|c| matches!(c, '\r' | '\n' | '\0')),
            "invalid subject"
        );
        ensure!(
            self.text.len() <= MAX_TEXT && !self.text.contains('\0'),
            "invalid text body"
        );
        Ok(())
    }
}

fn address(value: &str) -> Result<()> {
    ensure!(
        value.len() <= 254 && value.is_ascii(),
        "invalid mailbox address"
    );
    let (local, domain) = value
        .split_once('@')
        .ok_or_else(|| anyhow::anyhow!("invalid mailbox address"))?;
    ensure!(
        !local.is_empty()
            && local.len() <= 64
            && !local.starts_with('.')
            && !local.ends_with('.')
            && !local.contains("..")
            && local
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".!#$%&'*+-/=?^_`{|}~".contains(&b)),
        "invalid mailbox local part"
    );
    ensure!(
        !domain.is_empty()
            && domain.split('.').all(|label| !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')),
        "invalid mailbox domain"
    );
    Ok(())
}

enum Admission {
    New,
    Replay(String),
    Uncertain,
}

fn lookup(
    conn: &Connection,
    account: &str,
    operation: &str,
    digest: &str,
) -> Result<Option<Admission>> {
    let existing: Option<(String, Option<String>)> = conn.query_row(
        "SELECT payload_hash, receipt FROM bus_submissions WHERE account=?1 AND operation_id=?2",
        params![account, operation], |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional()?;
    if let Some((prior, receipt)) = existing {
        ensure!(
            prior == digest,
            "operation_id conflicts with previous payload"
        );
        return Ok(Some(
            receipt.map_or(Admission::Uncertain, Admission::Replay),
        ));
    }
    Ok(None)
}

fn reserve(
    conn: &Connection,
    account: &str,
    actor: &str,
    operation: &str,
    digest: &str,
) -> Result<Admission> {
    if let Some(existing) = lookup(conn, account, operation, digest)? {
        return Ok(existing);
    }
    conn.execute(
        "INSERT INTO bus_submissions(account,actor,operation_id,payload_hash) VALUES(?1,?2,?3,?4)",
        params![account, actor, operation, digest],
    )?;
    Ok(Admission::New)
}

pub async fn dispatch(
    cmd: &IncomingCommand,
    db: &Db,
    store: &Arc<SqliteMailStore>,
    aliases: &Arc<Runtime>,
    max_message_size: usize,
) -> (u8, String) {
    match submit(cmd, db, store, aliases, max_message_size).await {
        Ok(receipt) => (0, receipt),
        Err(error) => (
            10,
            json!({"error":error.to_string(),"delivery_confirmed":false}).to_string(),
        ),
    }
}

async fn submit(
    cmd: &IncomingCommand,
    db: &Db,
    store: &Arc<SqliteMailStore>,
    aliases: &Arc<Runtime>,
    max_message_size: usize,
) -> Result<String> {
    ensure!(
        !cmd.from.is_empty() && cmd.from.len() <= 256,
        "missing native Bus actor"
    );
    ensure!(
        cmd.body.len() <= MAX_REQUEST
            && cmd
                .header("args")
                .is_none_or(|args| args.len() <= MAX_REQUEST),
        "submission request exceeds byte bound"
    );
    let request: Request =
        serde_json::from_value(super::try_resolve_args(cmd).map_err(anyhow::Error::msg)?)?;
    request.validate()?;
    // Existing outcomes precede mutable account, alias, mailbox and size
    // checks. They report an earlier send, never authorise a new one.
    let digest = blake3::hash(&serde_json::to_vec(&request)?)
        .to_hex()
        .to_string();
    let canonical_account = request.account.to_ascii_lowercase();
    let conn = db.conn.clone();
    let prior_account = canonical_account.clone();
    let prior_digest = digest.clone();
    let prior_operation = request.operation_id.clone();
    let prior = tokio::task::spawn_blocking(move || {
        let conn = conn
            .lock()
            .map_err(|_| anyhow::anyhow!("submission database lock poisoned"))?;
        lookup(&conn, &prior_account, &prior_operation, &prior_digest)
    })
    .await??;
    match prior {
        Some(Admission::Replay(receipt)) => return Ok(receipt),
        Some(Admission::Uncertain) => anyhow::bail!(
            "submission outcome uncertain; operation is reserved and will not be resent"
        ),
        None => {}
        Some(Admission::New) => unreachable!("lookup never admits new work"),
    }
    let account = crate::db::account::get_by_email(&db.conn, &request.account.to_ascii_lowercase())
        .await?
        .ok_or_else(|| anyhow::anyhow!("submitting account not found"))?;
    ensure!(
        !account.password.starts_with('!'),
        "submitting account is locked"
    );
    let from = request.from.as_deref().unwrap_or(&account.email);
    ensure!(
        crate::props::aliases::sender_authorized(aliases, from, &account.email).await,
        "sender is not authorised for submitting account"
    );
    let message_id = format!(
        "{}.{}@{}",
        blake3::hash(account.email.as_bytes()).to_hex(),
        blake3::hash(request.operation_id.as_bytes()).to_hex(),
        from.split_once('@').expect("validated address").1
    );
    let bytes = mail_builder::MessageBuilder::new()
        .from(from)
        .to(request.to.iter().map(String::as_str).collect::<Vec<_>>())
        .subject(request.subject.as_str())
        .message_id(message_id.as_str())
        .text_body(request.text.as_str())
        .write_to_vec()?;
    ensure!(
        bytes.len() <= max_message_size,
        "rendered message exceeds server byte bound"
    );
    let ms = store.clone();
    let account_id = account.id;
    let drafts = tokio::task::spawn_blocking(move || -> Result<_> {
        ms.mailbox_by_role(account_id, MailboxRole::Drafts)?
            .ok_or_else(|| anyhow::anyhow!("submitting account has no Drafts mailbox"))
    })
    .await??;
    // Hash the typed request, not generated Date/Message-ID bytes. Identical
    // retries remain identical after reconnect or process restart.
    let actor = cmd.from.clone();
    let operation = request.operation_id.clone();
    let conn = db.conn.clone();
    let reservation = tokio::task::spawn_blocking(move || {
        let conn = conn
            .lock()
            .map_err(|_| anyhow::anyhow!("submission database lock poisoned"))?;
        reserve(&conn, &canonical_account, &actor, &operation, &digest)
    })
    .await??;
    match reservation {
        Admission::Replay(receipt) => return Ok(receipt),
        Admission::Uncertain => anyhow::bail!(
            "submission outcome uncertain; operation is reserved and will not be resent"
        ),
        Admission::New => {}
    }
    // After reservation, every failure is potentially ambiguous. Keep the
    // reservation permanently; caller retries may inspect, never resend it.
    let ms = store.clone();
    let account_id = account.id;
    let email_id = tokio::task::spawn_blocking(move || -> Result<_> {
        let hash = ms.mds().put_blob(&bytes)?;
        let parsed = mail_parser::MessageParser::default()
            .parse(&bytes)
            .ok_or_else(|| anyhow::anyhow!("rendered message parse failed"))?;
        let (id, _) = ms.create_email(
            account_id,
            drafts,
            hash,
            crate::jmap::email::build_envelope(&parsed),
            &[],
            Flags(0),
            Tags::new(),
            chrono::Utc::now().timestamp_millis(),
        )?;
        Ok(id.0)
    })
    .await??;
    let submission_id = crate::jmap::submission::create_submission_strict(db, store, aliases, account_id,
        &json!({"emailId":email_id.to_string(),"envelope":{"mailFrom":{"email":from},
            "rcptTo":request.to.iter().map(|email| json!({"email":email})).collect::<Vec<Value>>()}})).await?;
    let receipt = json!({"operation_id":request.operation_id,"status":"accepted",
        "submission_id":submission_id.to_string(),"email_id":email_id.to_string(),
        "message_id":message_id,"recipient_count":request.to.len(),"delivery_confirmed":false})
    .to_string();
    let saved = receipt.clone();
    let canonical_account = request.account.to_ascii_lowercase();
    let operation = request.operation_id;
    let conn = db.conn.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let conn = conn.lock().map_err(|_| anyhow::anyhow!("submission database lock poisoned"))?;
        ensure!(conn.execute("UPDATE bus_submissions SET receipt=?3 WHERE account=?1 AND operation_id=?2 AND receipt IS NULL",
            params![canonical_account,operation,saved])? == 1, "submission receipt persistence failed");
        Ok(())
    }).await??;
    Ok(receipt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn native_submission_uses_real_cas_local_delivery_and_queue_once() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::connect(
            tmp.path().join("mail.sqlite").to_str().unwrap(),
            tmp.path().join("blobs").to_str().unwrap(),
        )
        .await
        .unwrap();
        db.migrate().await.unwrap();
        {
            let conn = db.conn.lock().unwrap();
            conn.execute("INSERT INTO accounts(email,password,name) VALUES('sender@example.test','test-hash','Sender'),('local@example.test','test-hash','Local')", []).unwrap();
        }
        let sender = crate::db::account::get_by_email(&db.conn, "sender@example.test")
            .await
            .unwrap()
            .unwrap();
        let local = crate::db::account::get_by_email(&db.conn, "local@example.test")
            .await
            .unwrap()
            .unwrap();
        let mds = Arc::new(cosmix_mds::SqliteCasMds::open(&tmp.path().join("mds")).unwrap());
        let store = Arc::new(SqliteMailStore::new(mds));
        for account in [sender.id, local.id] {
            crate::props::accounts::seed_default_mailboxes_idempotent(&store, account).unwrap();
        }
        let aliases = Arc::new(Runtime::new(
            "maild",
            crate::props::aliases::spec(cosmix_props::Hooks::noop()),
            Arc::new(cosmix_props::MemoryStore::new("maild")),
        ));
        let mut cmd = IncomingCommand {
            from: "native-test".into(),
            command: "maild.submit".into(),
            id: None,
            args: Value::Null,
            headers: Default::default(),
            body: json!({"operation_id":"once-1",
                "account":"sender@example.test","to":["local@example.test","remote@outside.test"],
                "subject":"Native test","text":"Plain text ✓"})
            .to_string(),
        };
        assert_eq!(dispatch(&cmd, &db, &store, &aliases, 1).await.0, 10);
        assert_eq!(
            db.conn
                .lock()
                .unwrap()
                .query_row("SELECT count(*) FROM bus_submissions", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        let first = dispatch(&cmd, &db, &store, &aliases, 1024 * 1024).await;
        assert_eq!(first.0, 0, "{}", first.1);
        assert_eq!(dispatch(&cmd, &db, &store, &aliases, 1).await, first);
        assert_eq!(
            dispatch(&cmd, &db, &store, &aliases, 1024 * 1024).await,
            first
        );
        cmd.from = "reconnected-native-test".into();
        assert_eq!(
            dispatch(&cmd, &db, &store, &aliases, 1024 * 1024).await,
            first
        );
        let queued = crate::smtp::queue::list(&db.conn, 10).await.unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].to_addrs, vec!["remote@outside.test"]);
        let bytes = store.mds().get_blob(&queued[0].blob_hash.unwrap()).unwrap();
        let parsed = mail_parser::MessageParser::default().parse(&bytes).unwrap();
        assert!(parsed.body_text(0).unwrap().contains("Plain text ✓"));
        let inbox = store
            .mailbox_by_role(local.id, MailboxRole::Inbox)
            .unwrap()
            .unwrap();
        assert_eq!(
            store
                .list_emails_in_mailbox(local.id, inbox, crate::mailstore::ListOpts::default())
                .unwrap()
                .len(),
            1
        );
        let mut changed: Value = serde_json::from_str(&cmd.body).unwrap();
        changed["text"] = json!("different");
        cmd.body = changed.to_string();
        assert_eq!(
            dispatch(&cmd, &db, &store, &aliases, 1024 * 1024).await.0,
            10
        );
        assert_eq!(
            crate::smtp::queue::list(&db.conn, 10).await.unwrap().len(),
            1
        );
        db.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE accounts SET password='!locked' WHERE id=?1",
                params![sender.id],
            )
            .unwrap();
        changed["operation_id"] = json!("locked-2");
        cmd.body = changed.to_string();
        assert_eq!(
            dispatch(&cmd, &db, &store, &aliases, 1024 * 1024).await.0,
            10
        );
        assert_eq!(
            crate::smtp::queue::list(&db.conn, 10).await.unwrap().len(),
            1
        );
    }

    #[test]
    fn typed_request_rejects_injection_unknown_fields_and_unbounded_inputs() {
        let good = json!({"operation_id":"owned-1","account":"operator@example.test","to":["recipient@example.test"],"subject":"Native instructions","text":"Unicode ✓\nline two"});
        serde_json::from_value::<Request>(good.clone())
            .unwrap()
            .validate()
            .unwrap();
        for (field, value) in [
            ("subject", json!("hello\r\nBcc: forged@example.test")),
            ("to", json!(["recipient@example.test\r\nDATA"])),
            ("operation_id", json!("bad id")),
            ("text", json!("x".repeat(MAX_TEXT + 1))),
            ("raw", json!("not allowed")),
        ] {
            let mut bad = good.clone();
            bad[field] = value;
            if let Ok(request) = serde_json::from_value::<Request>(bad) {
                assert!(request.validate().is_err());
            }
        }
    }

    #[test]
    fn reservation_fences_replay_conflicts_and_ambiguous_restarts() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE bus_submissions(account TEXT,actor TEXT,operation_id TEXT,payload_hash TEXT,receipt TEXT,PRIMARY KEY(account,operation_id));").unwrap();
        assert!(matches!(
            reserve(&conn, "account", "actor", "op", "hash").unwrap(),
            Admission::New
        ));
        assert!(matches!(
            reserve(&conn, "account", "new-actor", "op", "hash").unwrap(),
            Admission::Uncertain
        ));
        assert!(reserve(&conn, "account", "new-actor", "op", "other").is_err());
        conn.execute("UPDATE bus_submissions SET receipt='accepted'", [])
            .unwrap();
        assert!(
            matches!(reserve(&conn,"account","new-actor","op","hash").unwrap(),Admission::Replay(value) if value=="accepted")
        );
        assert!(matches!(
            reserve(&conn, "other-account", "actor", "op", "hash").unwrap(),
            Admission::New
        ));
    }
}
