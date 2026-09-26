//! P4 storage and JMAP regressions. No broker or external service required.
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use base64::Engine;
use cosmix_maild::{
    config::Config,
    db, jmap,
    mailstore::{EmailEnvelope, MailStore},
    runtime::{BuiltMaild, RuntimeOpts, build_runtime},
};
use cosmix_mds::{ContainerAttrs, ContainerId, Flags, Mds, Tags};
use serde_json::{Value, json};

struct Fixture {
    built: BuiltMaild,
    _dir: tempfile::TempDir,
    inbox: ContainerId,
}

async fn migrate(f: &Fixture, args: Value) -> (u8, Value) {
    let state = f.state();
    let (rc, body) = cosmix_maild::bus::blobs::migrate(&state.db, &state.mailstore, args).await;
    (rc, serde_json::from_str(&body).unwrap())
}

fn item_count(f: &Fixture) -> i64 {
    let state = f.state();
    state
        .mailstore
        .mds()
        .with_set_tx(&cosmix_maild::mailstore::account_id_to_setid(1), |tx| {
            tx.tx()
                .query_row("SELECT COUNT(*) FROM item", [], |r| r.get(0))
                .map_err(|e| cosmix_mds::Error::Other(e.to_string()))
        })
        .unwrap()
}

#[tokio::test]
async fn legacy_migration_preserves_uuid_pins_and_queue_without_deleting() {
    let f = Fixture::new().await;
    let state = f.state();
    let bytes = b"Subject: legacy\r\n\r\nold upload\r\n";
    let id = db::blob::store(&state.db.conn, &state.db.blob_dir, 1, bytes)
        .await
        .unwrap();
    state.db.conn.lock().unwrap().execute(
        "INSERT INTO smtp_queue (from_addr, to_addrs, blob_id) VALUES ('sender@example.test', '[]', ?1)",
        [id.to_string()],
    ).unwrap();
    let hash = cosmix_mds::blob::hash_bytes(bytes);
    let before = item_count(&f);
    let (rc, dry) = migrate(&f, json!({})).await;
    assert_eq!(rc, 0, "{dry}");
    assert_eq!(dry["accounts"]["1"]["planned"], 1);
    assert_eq!(dry["done"], true);
    assert_eq!(item_count(&f), before);
    assert!(!state.mailstore.mds().blob_exists(&hash).unwrap());
    assert!(matches!(
        state
            .mailstore
            .resolve_blob_ref(1, cosmix_maild::mailstore::BlobId(id))
            .unwrap(),
        cosmix_maild::mailstore::BlobRefLookup::NotFound
    ));

    let (rc, applied) = migrate(&f, json!({"apply": true})).await;
    assert_eq!(rc, 0, "{applied}");
    assert_eq!(applied["accounts"]["1"]["migrated"], 1);
    assert_eq!(item_count(&f), before + 1);
    assert!(
        matches!(state.mailstore.resolve_blob_ref(1, cosmix_maild::mailstore::BlobId(id)).unwrap(),
        cosmix_maild::mailstore::BlobRefLookup::Found(h) if h == hash)
    );
    assert_eq!(
        f.download(1, &id.to_string()).await,
        (StatusCode::OK, bytes.to_vec())
    );
    assert_eq!(
        f.download(2, &id.to_string()).await.0,
        StatusCode::NOT_FOUND
    );
    let queue_hash: String = state
        .db
        .conn
        .lock()
        .unwrap()
        .query_row("SELECT blob_hash FROM smtp_queue", [], |r| r.get(0))
        .unwrap();
    assert_eq!(queue_hash, cosmix_mds::blob::hex(&hash));
    let (rc, again) = migrate(&f, json!({"apply": true})).await;
    assert_eq!(rc, 0, "{again}");
    assert_eq!(again["accounts"]["1"]["already_migrated"], 1);
    assert_eq!(item_count(&f), before + 1);
    cosmix_maild::mailstore::expiry::sweep_blocking(state.mailstore.mds(), i64::MAX / 2).unwrap();
    state.mailstore.mds().gc(false).unwrap(); // no zero-ref candidates: no quiescence sleep
    assert_eq!(item_count(&f), before + 1);
    assert_eq!(state.mailstore.mds().get_blob(&hash).unwrap(), bytes);
    assert_eq!(
        db::blob::load(&state.db.conn, &state.db.blob_dir, id)
            .await
            .unwrap()
            .unwrap(),
        bytes
    );
    let imported = jmap::email::import(
        &state.db,
        &state.mailstore,
        1,
        json!({"emails": {"old": {"blobId": id.to_string(),
            "mailboxIds": {(f.inbox.0.to_string()): true}}}}),
    )
    .await
    .unwrap();
    assert!(imported["created"]["old"]["id"].is_string(), "{imported}");
}

#[tokio::test]
async fn migration_pages_and_refusals_keep_bad_rows_untouched() {
    let f = Fixture::new().await;
    let state = f.state();
    for bytes in [b"one".as_slice(), b"two", b"three"] {
        db::blob::store(&state.db.conn, &state.db.blob_dir, 1, bytes)
            .await
            .unwrap();
    }
    let (rc, first) = migrate(&f, json!({"limit": 2})).await;
    assert_eq!(rc, 0);
    assert_eq!(first["done"], false);
    let (rc, last) = migrate(&f, json!({"limit": 2, "cursor": first["next"]})).await;
    assert_eq!(rc, 0);
    assert_eq!(last["done"], true);
    assert_eq!(last["accounts"]["1"]["planned"], 1);
    assert_eq!(item_count(&f), 0);
    let two = cosmix_mds::blob::hash_bytes(b"two");
    std::fs::write(
        cosmix_mds::blob::blob_path(&state.db.blob_dir, &two),
        b"bad",
    )
    .unwrap();
    let three = cosmix_mds::blob::hash_bytes(b"three");
    std::fs::remove_file(cosmix_mds::blob::blob_path(&state.db.blob_dir, &three)).unwrap();
    let (rc, failed) = migrate(&f, json!({"apply": true})).await;
    assert_eq!(rc, 10);
    assert_eq!(failed["accounts"]["1"]["migrated"], 1);
    assert_eq!(failed["accounts"]["1"]["corrupt"], 1);
    assert_eq!(failed["accounts"]["1"]["missing"], 1);
    assert_eq!(item_count(&f), 1);
    assert!(!state.mailstore.mds().blob_exists(&two).unwrap());
    for args in [
        json!({"limit": 0}),
        json!({"limit": 501}),
        json!({"cursor": -1}),
        json!({"apply": "yes"}),
        json!({"unexpected": true}),
        json!([]),
    ] {
        let (rc, reply) = migrate(&f, args).await;
        assert_eq!(rc, 10);
        assert!(
            reply["error"]
                .as_str()
                .unwrap()
                .starts_with("invalid_arguments:")
        );
    }
}

#[tokio::test]
async fn migration_retries_after_queue_failure_without_an_extra_hold() {
    let f = Fixture::new().await;
    let state = f.state();
    let id = db::blob::store(&state.db.conn, &state.db.blob_dir, 1, b"queued")
        .await
        .unwrap();
    {
        let conn = state.db.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO smtp_queue (from_addr, blob_id) VALUES ('sender@example.test', ?1)",
            [id.to_string()],
        )
        .unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_queue BEFORE UPDATE ON smtp_queue \
            BEGIN SELECT RAISE(ABORT, 'test queue failure'); END;",
        )
        .unwrap();
    }
    assert_eq!(migrate(&f, json!({"apply": true})).await.0, 10);
    assert_eq!(item_count(&f), 1);
    state
        .db
        .conn
        .lock()
        .unwrap()
        .execute_batch("DROP TRIGGER fail_queue")
        .unwrap();
    let (rc, result) = migrate(&f, json!({"apply": true})).await;
    assert_eq!(rc, 0, "{result}");
    assert_eq!(item_count(&f), 1);
    let hash: Option<String> = state
        .db
        .conn
        .lock()
        .unwrap()
        .query_row("SELECT blob_hash FROM smtp_queue", [], |r| r.get(0))
        .unwrap();
    assert!(hash.is_some());
}

#[tokio::test]
async fn migration_refuses_conflicting_uuid_alias_before_copy() {
    let f = Fixture::new().await;
    let state = f.state();
    let id = db::blob::store(&state.db.conn, &state.db.blob_dir, 1, b"legacy")
        .await
        .unwrap();
    let set = cosmix_maild::mailstore::account_id_to_setid(1);
    state
        .mailstore
        .mds()
        .with_set_tx(&set, |tx| {
            tx.tx()
                .execute(
                    "INSERT INTO blob_refs (blob_id, account_id, blob_hash, created_at) \
            VALUES (?1, 1, ?2, 0)",
                    rusqlite::params![id.to_string(), "a".repeat(64)],
                )
                .map_err(|e| cosmix_mds::Error::Other(e.to_string()))?;
            Ok(())
        })
        .unwrap();
    let (rc, result) = migrate(&f, json!({"apply": true})).await;
    assert_eq!(rc, 10);
    assert_eq!(result["accounts"]["1"]["conflicting"], 1);
    assert_eq!(item_count(&f), 0);
    assert!(
        !state
            .mailstore
            .mds()
            .blob_exists(&cosmix_mds::blob::hash_bytes(b"legacy"))
            .unwrap()
    );
}

impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = |name| dir.path().join(name).to_string_lossy().into_owned();
        let cfg = Config {
            database_path: path("mail.db"),
            blob_dir: path("legacy"),
            mds_dir: path("mds"),
            spam_db_dir: Some(path("spam")),
            rule_stats_dir: Some(path("rules")),
            smtp_inbound: None,
            smtp_smtps: None,
            imap_imaps: None,
            spam_enabled: Some(false),
            ..Config::default()
        };
        let built = build_runtime(
            &cfg,
            RuntimeOpts {
                enable_bus: false,
                disable_outbound_delivery: true,
                ..RuntimeOpts::default()
            },
        )
        .await
        .unwrap();
        let password = bcrypt::hash("test", 4).unwrap();
        for id in [1, 2] {
            built
                .app_state
                .db
                .conn
                .lock()
                .unwrap()
                .execute(
                    "INSERT INTO accounts (id, email, password) VALUES (?1, ?2, ?3)",
                    rusqlite::params![id, format!("user{id}@example.test"), password],
                )
                .unwrap();
            built.app_state.mailstore.ensure_account_set(id).unwrap();
        }
        let ms = &built.app_state.mailstore;
        let inbox = ms
            .mds()
            .create_container(
                &cosmix_maild::mailstore::account_id_to_setid(1),
                None,
                "Inbox",
                ContainerAttrs {
                    special_use: Some("\\Inbox".into()),
                    subscribed: true,
                    extra: json!({}),
                },
            )
            .unwrap();
        Self {
            built,
            _dir: dir,
            inbox,
        }
    }

    fn state(&self) -> Arc<jmap::AppState> {
        self.built.app_state.clone()
    }

    fn headers(account: i32) -> HeaderMap {
        let token = base64::engine::general_purpose::STANDARD
            .encode(format!("user{account}@example.test:test"));
        let mut headers = HeaderMap::new();
        headers.insert("authorization", format!("Basic {token}").parse().unwrap());
        headers
    }

    async fn download(&self, account: i32, id: &str) -> (StatusCode, Vec<u8>) {
        let response =
            jmap::blob_download(State(self.state()), Self::headers(account), Path(id.into()))
                .await
                .into_response();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 70 * 1024 * 1024)
            .await
            .unwrap();
        (status, bytes.to_vec())
    }

    fn deliver(&self, bytes: &[u8]) -> cosmix_mds::ItemId {
        let ms = &self.built.app_state.mailstore;
        let hash = ms.mds().put_blob(bytes).unwrap();
        ms.create_email(
            1,
            self.inbox,
            hash,
            EmailEnvelope {
                from: "sender@example.test".into(),
                to: vec!["user1@example.test".into()],
                cc: vec![],
                bcc: vec![],
                reply_to: vec![],
                subject: "P4".into(),
                date: 0,
                message_id: None,
            },
            &[],
            Flags(0),
            Tags::new(),
            0,
        )
        .unwrap()
        .0
    }
}

#[tokio::test]
async fn upload_without_legacy_write_downloads_and_imports() {
    let f = Fixture::new().await;
    let bytes = b"From: sender@example.test\r\nSubject: P4\r\n\r\nupload\r\n";
    let response = jmap::blob_upload(
        State(f.state()),
        Fixture::headers(1),
        Path("1".into()),
        axum::body::Bytes::from_static(bytes),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::CREATED);
    let value: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 8192)
            .await
            .unwrap(),
    )
    .unwrap();
    let id = value["blobId"].as_str().unwrap();
    let hash = blake3::hash(bytes).to_hex().to_string();
    for download_id in [id, hash.as_str()] {
        assert_eq!(
            f.download(1, download_id).await,
            (StatusCode::OK, bytes.to_vec())
        );
        assert_eq!(f.download(2, download_id).await.0, StatusCode::NOT_FOUND);
    }
    let count: i64 = f
        .state()
        .db
        .conn
        .lock()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM blobs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0);
    assert_eq!(
        std::fs::read_dir(&f.state().db.blob_dir).unwrap().count(),
        0
    );
    let state = f.state();
    let imported = jmap::email::import(
        &state.db,
        &state.mailstore,
        1,
        json!({"emails": {"new": {"blobId": id,
            "mailboxIds": {(f.inbox.0.to_string()): true}}}}),
    )
    .await
    .unwrap();
    assert!(imported["created"]["new"]["id"].is_string(), "{imported}");
}

#[tokio::test]
async fn delivered_hash_and_legacy_fallback_are_account_scoped() {
    let f = Fixture::new().await;
    let bytes = b"Subject: delivered\r\n\r\nbody\r\n";
    f.deliver(bytes);
    let hash = blake3::hash(bytes).to_hex().to_string();
    assert_eq!(f.download(1, &hash).await, (StatusCode::OK, bytes.to_vec()));
    assert_eq!(f.download(2, &hash).await.0, StatusCode::NOT_FOUND);
    let state = f.state();
    let legacy = db::blob::store(&state.db.conn, &state.db.blob_dir, 2, bytes)
        .await
        .unwrap();
    assert_eq!(f.download(2, &hash).await.0, StatusCode::OK);
    assert_eq!(f.download(2, &legacy.to_string()).await.0, StatusCode::OK);
    assert_eq!(
        f.download(1, &legacy.to_string()).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn expired_dangling_and_unowned_cas_do_not_grant_hash_access() {
    let f = Fixture::new().await;
    let ms = &f.built.app_state.mailstore;
    let hash = ms.mds().put_blob(b"unowned").unwrap();
    assert!(!ms.owns_blob_hash(1, &hash).unwrap());
    assert!(!ms.owns_blob_hash(99, &hash).unwrap());
    let alias = ms.create_blob_ref(1, hash, 7, 1).unwrap();
    assert!(!ms.owns_blob_hash(1, &hash).unwrap());
    let set = cosmix_maild::mailstore::account_id_to_setid(1);
    ms.mds()
        .with_set_tx(&set, |tx| {
            tx.tx()
                .execute("UPDATE blob_refs SET expires_at = ?1", [i64::MAX])
                .map_err(|e| cosmix_mds::Error::Other(e.to_string()))?;
            Ok(())
        })
        .unwrap();
    assert!(ms.owns_blob_hash(1, &hash).unwrap());
    std::fs::remove_file(cosmix_mds::blob::blob_path(&ms.mds().blobs_root(), &hash)).unwrap();
    assert!(!ms.owns_blob_hash(1, &hash).unwrap());
    assert_ne!(f.download(1, &alias.to_string()).await.0, StatusCode::OK);
    ms.mds()
        .with_set_tx(&set, |tx| {
            tx.tx()
                .execute_batch("DROP TABLE blob_refs")
                .map_err(|e| cosmix_mds::Error::Other(e.to_string()))?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        f.download(1, &cosmix_mds::blob::hex(&hash)).await.0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
}
