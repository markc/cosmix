//! Recovery-file tests (ced E1 plan §5, Stage E1c). Two layers:
//! - the daemon in-process (`Editd::start_recovering` over a temp recovery
//!   dir, a recording sink, synthesized commands) for lifecycle, restore and
//!   restored-buffer state; a "crash" is `Recovery::halt` (the writer stops
//!   where it stands) followed by a fresh start on the same directory;
//! - the [`Writer`] driven directly, with injected crash points, for the
//!   generation-switch windows and the §5.1 interleaving fixture.
//!
//! Waits are the flush barrier or deadline-bounded sink waits — never sleeps.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use cosmix_client::IncomingCommand;
use cosmix_edit_core::buffer::Eol;
use cosmix_edit_core::ot::Edit;
use cosmix_editd::events::EventSink;
use cosmix_editd::events::testing::RecordingSink;
use cosmix_editd::limits::{MAX_BUFFERS, MAX_TOTAL_BYTES};
use cosmix_editd::recovery::{
    self, Fault, MAX_RECOVERY_QUEUE_BYTES, Recovery, RecoveryConfig, RecoveryMeta, RecoveryMsg, RestoreCaps,
    RestoredBuffer, Shared, Writer,
};
use cosmix_editd::{Config, Editd};
use serde_json::{Value, json};

const CAPS: RestoreCaps = RestoreCaps { max_buffers: MAX_BUFFERS, max_bytes: MAX_TOTAL_BYTES };

struct H {
    editd: Arc<Editd>,
    sink: Arc<RecordingSink>,
    rec: Arc<Recovery>,
    restored: Vec<RestoredBuffer>,
}

/// Start a daemon over `recdir` (restore first, as `serve` does).
async fn boot(recdir: &Path, epoch: &str) -> H {
    let (dir, e) = (recdir.to_path_buf(), epoch.to_string());
    let (rec, restored) = tokio::task::spawn_blocking(move || Recovery::start(&dir, &e, CAPS)).await.unwrap();
    let sink = Arc::new(RecordingSink::default());
    let config = Config { epoch: epoch.into(), mesh_open: true, budget_cap: MAX_TOTAL_BYTES };
    let editd = Editd::start_recovering(config, rec.clone(), restored.clone(), sink.clone() as Arc<dyn EventSink>).await;
    H { editd, sink, rec, restored }
}

fn cmd(verb: &str, args: Value) -> IncomingCommand {
    let mut headers = BTreeMap::new();
    headers.insert("broker_origin".to_string(), "local".to_string());
    IncomingCommand { from: "tester".into(), command: verb.into(), id: Some("1".into()), args, body: String::new(), headers }
}

impl H {
    async fn call(&self, verb: &str, args: Value) -> (u8, Value) {
        let (rc, body) = self.editd.handle(&cmd(verb, args)).await;
        (rc, serde_json::from_str(&body).unwrap_or(Value::String(body)))
    }

    async fn ok(&self, verb: &str, args: Value) -> Value {
        let (rc, v) = self.call(verb, args).await;
        assert_eq!(rc, 0, "{verb} refused: {v}");
        v
    }

    async fn open(&self, path: &Path) -> (String, String) {
        let v = self.ok("edit.open", json!({"path": path})).await;
        (v["buffer"].as_str().unwrap().to_string(), v["recovery_id"].as_str().unwrap().to_string())
    }

    async fn scratch(&self) -> (String, String) {
        let v = self.ok("edit.open", json!({})).await;
        (v["buffer"].as_str().unwrap().to_string(), v["recovery_id"].as_str().unwrap().to_string())
    }

    async fn text(&self, b: &str) -> String {
        self.ok("edit.get", json!({"buffer": b})).await["text"].as_str().unwrap().to_string()
    }

    async fn flush(&self) -> Value {
        let v = self.ok("edit.recovery.flush", json!({})).await;
        assert_eq!(v["synced"], true, "flush: {v}");
        v
    }

    async fn list(&self) -> Vec<Value> {
        self.ok("edit.list", json!({})).await["buffers"].as_array().unwrap().clone()
    }

    /// The restored buffer that carries `rid`.
    fn restored(&self, rid: &str) -> &RestoredBuffer {
        self.restored.iter().find(|r| r.rid == rid).unwrap_or_else(|| panic!("{rid} not restored: {:?}", self.restored))
    }

    async fn event(&self, pred: impl Fn(&Value) -> bool) {
        let found = self
            .sink
            .wait_for(|s| s.iter().any(|(topic, body)| topic == "edit.changed" && pred(body)))
            .await;
        assert!(found, "event never arrived; sent: {:?}", self.sink.sent.lock().unwrap());
    }

    /// A crash: flush what is queued so the test is deterministic, then stop
    /// the writer (nothing further reaches the files).
    async fn crash(self) {
        self.flush().await;
        self.rec.halt();
    }
}

fn rid_files(recdir: &Path, rid: &str) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(recdir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(&format!("{rid}.")))
        .collect();
    v.sort();
    v
}

fn meta_of(recdir: &Path, rid: &str) -> RecoveryMeta {
    serde_json::from_slice(&std::fs::read(recdir.join(format!("{rid}.meta.json"))).unwrap()).unwrap()
}

fn dirs() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let (work, rec) = (root.join("work"), root.join("recovery"));
    std::fs::create_dir(&work).unwrap();
    (tmp, work, rec)
}

// ── lifecycle ───────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dirty_buffer_writes_files_and_a_durable_save_removes_them() {
    let (_t, work, recdir) = dirs();
    let h = boot(&recdir, "00000001").await;
    let info = h.ok("edit.info", json!({})).await;
    assert_eq!((info["volatile"].clone(), info["recovery"]["enabled"].clone()), (json!(false), json!(true)));
    assert_eq!(std::fs::metadata(&recdir).unwrap().permissions().mode() & 0o777, 0o700);

    let f = work.join("a.txt");
    std::fs::write(&f, "hello\n").unwrap();
    let (b, rid) = h.open(&f).await;
    h.flush().await;
    assert!(rid_files(&recdir, &rid).is_empty(), "a clean buffer has no files");

    h.ok("edit.insert", json!({"buffer": b, "at": 0, "text": "X"})).await;
    h.ok("edit.insert", json!({"buffer": b, "at": 1, "text": "Y"})).await;
    h.flush().await;
    let files = rid_files(&recdir, &rid);
    assert_eq!(files, vec![format!("{rid}.1.log"), format!("{rid}.1.snap"), format!("{rid}.meta.json")]);
    for name in &files {
        assert_eq!(std::fs::metadata(recdir.join(name)).unwrap().permissions().mode() & 0o777, 0o600, "{name}");
    }
    let meta = meta_of(&recdir, &rid);
    assert_eq!((meta.generation, meta.path.as_deref()), (1, Some(f.to_str().unwrap())));
    assert!(meta.base.is_some());
    // The clean→dirty snapshot is at rev 1; rev 2 is the one log record.
    let log = std::fs::read_to_string(recdir.join(format!("{rid}.1.log"))).unwrap();
    assert_eq!(log.lines().count(), 1, "{log}");
    assert!(log.contains("\"rev\":2"), "{log}");

    let saved = h.ok("edit.save", json!({"buffer": b})).await;
    assert_eq!(saved["durable"], true);
    h.flush().await;
    assert!(rid_files(&recdir, &rid).is_empty(), "a durable save removes the files");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clean_reload_or_a_force_close_removes_the_files() {
    let (_t, work, recdir) = dirs();
    let h = boot(&recdir, "00000001").await;
    let (s, srid) = h.scratch().await;
    h.ok("edit.insert", json!({"buffer": s, "at": 0, "text": "scratch"})).await;
    let f = work.join("r.txt");
    std::fs::write(&f, "disk\n").unwrap();
    let (b, brid) = h.open(&f).await;
    h.ok("edit.insert", json!({"buffer": b, "at": 0, "text": "mine "})).await;
    h.flush().await;
    assert_eq!(rid_files(&recdir, &srid).len(), 3);
    assert_eq!(rid_files(&recdir, &brid).len(), 3);

    h.ok("edit.reload", json!({"buffer": b, "force": true})).await;
    let r = h.ok("edit.close", json!({"buffer": s, "force": true})).await;
    assert_eq!(r["closed"], true);
    h.flush().await;
    assert!(rid_files(&recdir, &brid).is_empty(), "a clean reload removes the files");
    assert!(rid_files(&recdir, &srid).is_empty(), "an explicit discard removes the files");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_non_durable_save_keeps_the_files_through_a_close_and_restore_cleans_them() {
    let (_t, work, recdir) = dirs();
    let h = boot(&recdir, "00000001").await;
    let f = work.join("nd.txt");
    std::fs::write(&f, "one\n").unwrap();
    let (b, rid) = h.open(&f).await;
    h.ok("edit.insert", json!({"buffer": b, "at": 0, "text": "two "})).await;
    h.flush().await;
    let before = meta_of(&recdir, &rid);

    cosmix_editd::files::FAIL_DIR_FSYNC.lock().unwrap().push(work.clone());
    let saved = h.ok("edit.save", json!({"buffer": b})).await;
    cosmix_editd::files::FAIL_DIR_FSYNC.lock().unwrap().retain(|d| d != &work);
    assert_eq!(saved["durable"], false, "{saved}");
    h.flush().await;
    let after = meta_of(&recdir, &rid);
    assert!(after.generation > before.generation, "the base change re-switched: {after:?}");
    assert_ne!(after.base, before.base);

    // An ordinary close of the now-clean buffer never deletes them.
    assert_eq!(h.ok("edit.close", json!({"buffer": b})).await["closed"], true);
    h.flush().await;
    assert_eq!(rid_files(&recdir, &rid).len(), 3, "kept after a durable:false save + close");

    h.crash().await;
    let h = boot(&recdir, "00000002").await;
    let r = h.restored(&rid);
    assert!(r.clean, "content equal to disk restores clean");
    assert_eq!(r.text, "two one\n");
    h.flush().await;
    assert!(rid_files(&recdir, &rid).is_empty(), "restore cleaned them up");
    let row = h.list().await.into_iter().find(|e| e["recovery_id"] == rid.as_str()).unwrap();
    assert_eq!((row["dirty"].clone(), row["recovered"].clone()), (json!(false), json!(true)));
}

// ── restore ─────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_brings_back_path_and_scratch_buffers() {
    let (_t, work, recdir) = dirs();
    let h = boot(&recdir, "00000001").await;
    let f = work.join("p.txt");
    std::fs::write(&f, "body\n").unwrap();
    let (b, brid) = h.open(&f).await;
    h.ok("edit.insert", json!({"buffer": b, "at": 0, "text": "edited "})).await;
    let (s, srid) = h.scratch().await;
    h.ok("edit.insert", json!({"buffer": s, "at": 0, "text": "scratch "})).await;
    h.ok("edit.insert", json!({"buffer": s, "at": 8, "text": "text"})).await;
    h.crash().await;

    let h = boot(&recdir, "00000002").await;
    assert_eq!(h.restored.len(), 2);
    let info = h.ok("edit.info", json!({})).await;
    assert_eq!((info["recovery"]["restored"].clone(), info["volatile"].clone()), (json!(2), json!(false)));
    let p = h.restored(&brid).clone();
    let s2 = h.restored(&srid).clone();
    assert_eq!(h.text(&p.bid).await, "edited body\n");
    assert_eq!(h.text(&s2.bid).await, "scratch text");
    assert_eq!((s2.from.epoch.as_str(), s2.from.rev), ("00000001", 2));

    // Reopening the path finds the restored buffer, with where it came from.
    let v = h.ok("edit.open", json!({"path": f})).await;
    assert_eq!((v["buffer"].as_str(), v["reopened"].clone()), (Some(p.bid.as_str()), json!(true)));
    assert_eq!((v["recovered"].clone(), v["recovered_from"]["epoch"].clone()), (json!(true), json!("00000001")));
    assert_eq!(v["recovered_from"]["rev"], 1);
    for row in h.list().await {
        assert_eq!((row["dirty"].clone(), row["rev"].clone(), row["recovered"].clone()), (json!(true), json!(0), json!(true)));
    }
    let props = h.ok("edit.props.get", json!({})).await;
    assert_eq!(props["buffers"][p.bid.as_str()]["recovery_id"], brid.as_str());
    assert_eq!(props["lifecycle"]["recovery_ok"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_edit_restart_restore_carries_both_sets_of_edits() {
    let (_t, _work, recdir) = dirs();
    let h = boot(&recdir, "00000001").await;
    let (s, rid) = h.scratch().await;
    h.ok("edit.insert", json!({"buffer": s, "at": 0, "text": "pre"})).await;
    h.ok("edit.insert", json!({"buffer": s, "at": 3, "text": "-crash"})).await;
    h.crash().await;

    let h = boot(&recdir, "00000002").await;
    let r = h.restored(&rid).clone();
    assert_eq!(r.generation, 2, "restore started a new generation");
    assert_eq!(h.text(&r.bid).await, "pre-crash");
    // Post-restore revs 1, 2 go to the fresh generation's log.
    h.ok("edit.insert", json!({"buffer": r.bid, "at": 9, "text": " post"})).await;
    h.ok("edit.insert", json!({"buffer": r.bid, "at": 14, "text": "-restore"})).await;
    h.crash().await;

    let h = boot(&recdir, "00000003").await;
    let r = h.restored(&rid).clone();
    assert_eq!(h.text(&r.bid).await, "pre-crash post-restore");
    assert_eq!((r.from.epoch.as_str(), r.from.rev, r.generation), ("00000002", 2, 3));
}

// ── restored-buffer state ───────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restored_buffer_is_dirty_until_saved_and_never_auto_reloads() {
    let (_t, work, recdir) = dirs();
    let h = boot(&recdir, "00000001").await;
    let f = work.join("s.txt");
    std::fs::write(&f, "disk\n").unwrap();
    let (b, rid) = h.open(&f).await;
    h.ok("edit.insert", json!({"buffer": b, "at": 0, "text": "mine "})).await;
    h.crash().await;

    let h = boot(&recdir, "00000002").await;
    let r = h.restored(&rid).clone();
    assert_eq!((r.clean, r.disk), (false, cosmix_edit_core::wire::DiskState::Clean), "identity equal: disk clean, buffer dirty");
    // Unforced close by the last holder: refused, it is dirty at rev 0.
    let (rc, v) = h.call("edit.close", json!({"buffer": r.bid})).await;
    assert_eq!((rc, v["error_code"].clone(), v["reason"].clone()), (10, json!("CONFLICT"), json!("dirty")), "{v}");
    // A disk change marks it modified and is never auto-reloaded.
    std::fs::write(&f, "theirs\n").unwrap();
    h.event(|e| e["event"] == "disk" && e["buffer"] == r.bid.as_str() && e["disk"] == "modified").await;
    assert_eq!(h.text(&r.bid).await, "mine disk\n");
    h.crash().await;

    // An immediate restart restores it again.
    let h = boot(&recdir, "00000003").await;
    let r = h.restored(&rid).clone();
    assert_eq!(h.text(&r.bid).await, "mine disk\n");
    assert_eq!(r.disk, cosmix_edit_core::wire::DiskState::Modified, "identity differs from the recorded base now");
    // …so a plain save refuses.
    let (rc, v) = h.call("edit.save", json!({"buffer": r.bid})).await;
    assert_eq!((rc, v["reason"].clone()), (10, json!("disk_modified")), "{v}");
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "theirs\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn content_equal_to_disk_restores_clean_with_the_current_identity() {
    let (_t, work, recdir) = dirs();
    let h = boot(&recdir, "00000001").await;
    let f = work.join("e.txt");
    std::fs::write(&f, "same\n").unwrap();
    let (b, rid) = h.open(&f).await;
    h.ok("edit.insert", json!({"buffer": b, "at": 0, "text": "x"})).await;
    h.ok("edit.delete", json!({"buffer": b, "range": [0, 1]})).await;
    h.crash().await;
    // Same content, new identity (another inode).
    let tmp = work.join("e.tmp");
    std::fs::write(&tmp, "same\n").unwrap();
    std::fs::rename(&tmp, &f).unwrap();

    let h = boot(&recdir, "00000002").await;
    let r = h.restored(&rid).clone();
    assert!(r.clean);
    let row = h.list().await.into_iter().find(|e| e["buffer"] == r.bid.as_str()).unwrap();
    assert_eq!((row["dirty"].clone(), row["disk"].clone()), (json!(false), json!("clean")));
    h.flush().await;
    assert!(rid_files(&recdir, &rid).is_empty());
    // `base` is the CURRENT identity: a plain save goes through.
    h.ok("edit.insert", json!({"buffer": r.bid, "at": 0, "text": "!"})).await;
    let saved = h.ok("edit.save", json!({"buffer": r.bid})).await;
    assert_eq!(saved["durable"], true);
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "!same\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn identity_different_at_restore_refuses_a_plain_save() {
    let (_t, work, recdir) = dirs();
    let h = boot(&recdir, "00000001").await;
    let f = work.join("i.txt");
    std::fs::write(&f, "v1\n").unwrap();
    let (b, rid) = h.open(&f).await;
    h.ok("edit.insert", json!({"buffer": b, "at": 0, "text": "mine "})).await;
    h.crash().await;
    std::fs::write(&f, "someone else\n").unwrap();

    let h = boot(&recdir, "00000002").await;
    let r = h.restored(&rid).clone();
    assert_eq!(r.disk, cosmix_edit_core::wire::DiskState::Modified);
    let (rc, v) = h.call("edit.save", json!({"buffer": r.bid})).await;
    assert_eq!((rc, v["reason"].clone()), (10, json!("disk_modified")), "{v}");
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "someone else\n");
    // force still saves, durably, and that removes the files.
    h.ok("edit.save", json!({"buffer": r.bid, "force": true})).await;
    h.flush().await;
    assert!(rid_files(&recdir, &rid).is_empty());
}

// ── overflow, compaction, flush, shutdown ───────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overflow_repairs_at_once_and_flush_waits_for_the_repair() {
    let (_t, _work, recdir) = dirs();
    let h = boot(&recdir, "00000001").await;
    let (s, rid) = h.scratch().await;
    h.ok("edit.insert", json!({"buffer": s, "at": 0, "text": "a"})).await;
    h.flush().await;
    assert_eq!(meta_of(&recdir, &rid).generation, 1);

    // The queue budget is full: the next record cannot be reserved.
    let budget = &h.rec.shared.budget;
    assert!(budget.try_reserve(MAX_RECOVERY_QUEUE_BYTES - budget.used()));
    h.ok("edit.insert", json!({"buffer": s, "at": 1, "text": "b"})).await;
    // No further edit: the flush returns only once the repair Switch is durable.
    let flushed = h.flush().await;
    assert!(flushed["repairs"].as_u64().unwrap() >= 1, "{flushed}");
    assert_eq!(meta_of(&recdir, &rid).generation, 2, "the repair snapshot is the new generation");
    let info = h.ok("edit.info", json!({})).await;
    assert_eq!((info["recovery"]["ok"].clone(), info["volatile"].clone()), (json!(true), json!(false)), "{info}");
    budget.release(MAX_RECOVERY_QUEUE_BYTES);
    h.crash().await;

    let h = boot(&recdir, "00000002").await;
    assert_eq!(h.restored(&rid).text, "ab", "the dropped record is in the repair snapshot");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_log_past_the_threshold_compacts_with_no_further_edit() {
    let (_t, _work, recdir) = dirs();
    let h = boot(&recdir, "00000001").await;
    let (s, rid) = h.scratch().await;
    h.ok("edit.insert", json!({"buffer": s, "at": 0, "text": "x"})).await;
    let chunk = "y".repeat(600 * 1024);
    h.ok("edit.insert", json!({"buffer": s, "at": "end", "text": chunk})).await;
    h.ok("edit.insert", json!({"buffer": s, "at": "end", "text": chunk})).await;
    // The first flush lands the appends (the writer raises the compaction
    // request); the second waits for the Switch that answers it.
    h.flush().await;
    h.flush().await;
    assert_eq!(meta_of(&recdir, &rid).generation, 2, "compacted into a new generation");
    assert_eq!(std::fs::metadata(recdir.join(format!("{rid}.2.log"))).unwrap().len(), 0);
    assert!(!recdir.join(format!("{rid}.1.log")).exists(), "the old generation is retired");
    h.crash().await;
    let h = boot(&recdir, "00000002").await;
    assert_eq!(h.restored(&rid).text.len(), 1 + 2 * 600 * 1024);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_shutdown_drain_syncs_everything() {
    let (_t, _work, recdir) = dirs();
    let h = boot(&recdir, "00000001").await;
    let (s, rid) = h.scratch().await;
    for i in 0..50 {
        h.ok("edit.insert", json!({"buffer": s, "at": "end", "text": format!("{i},")})).await;
    }
    // What `serve` runs on SIGTERM, well inside the 1 s debounce.
    let drained = h.editd.recovery_flush().await.unwrap();
    assert!(drained.synced);
    assert_eq!(h.ok("edit.info", json!({})).await["recovery"]["unsynced"], 0);
    let want = h.text(&s).await;
    h.rec.halt();
    let h = boot(&recdir, "00000002").await;
    assert_eq!(h.restored(&rid).text, want);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recovery_off_is_volatile() {
    let off = RecoveryConfig::from_vars(Some("0"), Some("/r"), None, Some("/home/u"));
    assert!(!off.enabled);
    let c = RecoveryConfig::from_vars(None, None, Some("/state"), Some("/home/u"));
    assert_eq!((c.enabled, c.dir), (true, Some(PathBuf::from("/state/cosmix/edit/recovery"))));
    let c = RecoveryConfig::from_vars(None, None, Some("relative"), Some("/home/u"));
    assert_eq!(c.dir, Some(PathBuf::from("/home/u/.local/state/cosmix/edit/recovery")));
    let c = RecoveryConfig::from_vars(None, Some("/explicit"), Some("/state"), None);
    assert_eq!(c.dir, Some(PathBuf::from("/explicit")));
    assert!(!RecoveryConfig::from_vars(None, None, None, None).enabled);

    let sink = Arc::new(RecordingSink::default());
    let config = Config { epoch: "00000001".into(), mesh_open: true, budget_cap: MAX_TOTAL_BYTES };
    let editd = Editd::start(config, sink as Arc<dyn EventSink>);
    let (rc, body) = editd.handle(&cmd("edit.info", json!({}))).await;
    let info: Value = serde_json::from_str(&body).unwrap();
    assert_eq!((rc, info["volatile"].clone(), info["recovery"]["enabled"].clone()), (0, json!(true), json!(false)));
    let (rc, body) = editd.handle(&cmd("edit.recovery.flush", json!({}))).await;
    assert_eq!(rc, 0);
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["synced"], false);
    let props: Value = serde_json::from_str(&editd.handle(&cmd("edit.props.get", json!({}))).await.1).unwrap();
    assert_eq!((props["lifecycle"]["volatile"].clone(), props["lifecycle"]["recovery_ok"].clone()), (json!(true), json!(false)));
}

// ── the writer, driven directly ─────────────────────────────────────────────

fn meta(rid: &str) -> RecoveryMeta {
    RecoveryMeta {
        format: recovery::META_FORMAT.into(),
        rid: rid.into(),
        generation: 0,
        path: None,
        opened_as: None,
        language: "text".into(),
        eol: Eol::None,
        bom: false,
        base: None,
        epoch: "0000000a".into(),
        buffer: "b1_0000000a".into(),
        created_ms: 1,
    }
}

const RID: &str = "00112233445566aa";

fn switch(generation: u64, rev: u64, text: &str) -> RecoveryMsg {
    RecoveryMsg::Switch { rid: RID.into(), generation, rev, text: Arc::from(text), meta: Box::new(meta(RID)) }
}

/// Append `insert` at the end of `before`.
fn append(generation: u64, rev: u64, before: &str, insert: &str) -> RecoveryMsg {
    let edits = vec![Edit { offset: before.len(), delete: 0, insert: insert.into() }];
    RecoveryMsg::Append { rid: RID.into(), generation, rev, edits }
}

fn writer(dir: &Path) -> Writer {
    Writer::new(dir, Arc::new(Shared::default()))
}

fn restore_one(dir: &Path) -> Option<RestoredBuffer> {
    let mut w = writer(dir);
    let mut out = recovery::restore(&mut w, "0000000b", CAPS);
    assert!(out.len() <= 1);
    out.pop()
}

#[test]
fn generation_switch_crash_windows_give_old_or_new_never_less() {
    for step in 1..=6u8 {
        let t = tempfile::tempdir().unwrap();
        let mut w = writer(t.path());
        w.handle(switch(1, 0, "base"));
        w.handle(append(1, 1, "base", "+1"));
        w.sync();
        // Rev 2 was never appended (an overflow): the new snapshot holds it.
        w.set_fault(move |f| f == Fault::SwitchStep(step));
        w.handle(switch(2, 2, "base+1+2"));
        assert!(w.crashed(), "step {step} was reached");
        let r = restore_one(t.path()).expect("restorable at every step");
        let (text, rev) = if step <= 3 { ("base+1", 1) } else { ("base+1+2", 2) };
        assert_eq!((r.text.as_str(), r.from.rev), (text, rev), "crash after step {step}");
    }
}

/// §5.1 fixture (codex N4): Append g:58, the reservation for 59 fails,
/// Switch g+1@59, Append g+1:60 — a crash after every message and every
/// switch step restores exactly rev 58's, 59's or 60's text.
#[test]
fn repair_snapshot_interleaved_with_appends() {
    let text = |rev: u64| {
        let mut s = String::from("rev57\n");
        for r in 58..=rev {
            s.push_str(&format!("r{r};"));
        }
        s
    };
    // Fault events in order: Message(58) · SwitchStep 1..=6 · Message(switch) · Message(60).
    for crash_at in 0..=9usize {
        let t = tempfile::tempdir().unwrap();
        let mut w = writer(t.path());
        w.handle(switch(1, 57, &text(57)));
        let mut n = 0usize;
        w.set_fault(move |_| {
            let hit = n == crash_at;
            n += 1;
            hit
        });
        w.handle(append(1, 58, &text(57), "r58;"));
        // Rev 59: reservation failed, nothing sent; the switch covers it.
        w.handle(switch(2, 59, &text(59)));
        w.handle(append(2, 60, &text(59), "r60;"));
        assert_eq!(w.crashed(), crash_at < 9, "crash point {crash_at}");
        let r = restore_one(t.path()).expect("restorable");
        let want = match crash_at {
            0..=3 => 58,
            4..=7 => 59,
            _ => 60,
        };
        assert_eq!(r.from.rev, want, "crash point {crash_at}");
        assert_eq!(r.text, text(want), "crash point {crash_at}: no double-applied or missing rev");
        assert_eq!(r.dropped_records, 0);
    }
}

#[test]
fn an_append_for_a_stale_generation_is_discarded() {
    let t = tempfile::tempdir().unwrap();
    let mut w = writer(t.path());
    w.set_strict(false);
    w.handle(switch(3, 0, ""));
    w.handle(append(2, 1, "", "old gen"));
    w.handle(append(9, 1, "", "future gen"));
    assert_eq!(w.shared().stats.stale_appends.load(Ordering::Acquire), 2);
    assert_eq!(std::fs::metadata(t.path().join(format!("{RID}.3.log"))).unwrap().len(), 0);
    assert_eq!(w.shared().budget.used(), 0, "the reservation was released anyway");
}

fn log_path(dir: &Path, generation: u64) -> PathBuf {
    dir.join(format!("{RID}.{generation}.log"))
}

fn three_records(dir: &Path) {
    let mut w = writer(dir);
    w.handle(switch(1, 0, ""));
    w.handle(append(1, 1, "", "a"));
    w.handle(append(1, 2, "a", "b"));
    w.handle(append(1, 3, "ab", "c"));
    w.sync();
}

#[test]
fn a_torn_log_tail_is_salvaged() {
    let t = tempfile::tempdir().unwrap();
    three_records(t.path());
    let log = std::fs::read(log_path(t.path(), 1)).unwrap();
    std::fs::write(log_path(t.path(), 1), &log[..log.len() - 7]).unwrap();
    let r = restore_one(t.path()).unwrap();
    assert_eq!((r.text.as_str(), r.from.rev, r.dropped_records), ("ab", 2, 1));
}

#[test]
fn a_bad_record_hash_stops_the_replay_there() {
    let t = tempfile::tempdir().unwrap();
    three_records(t.path());
    let log = std::fs::read_to_string(log_path(t.path(), 1)).unwrap();
    let lines: Vec<&str> = log.lines().collect();
    let mut rec: Value = serde_json::from_str(lines[1]).unwrap();
    rec["h"] = json!("0000000000000000");
    let bad = format!("{}\n{}\n{}\n", lines[0], rec, lines[2]);
    std::fs::write(log_path(t.path(), 1), bad).unwrap();
    let r = restore_one(t.path()).unwrap();
    assert_eq!((r.text.as_str(), r.from.rev, r.dropped_records), ("a", 1, 2));
    // A rev gap stops it too.
    let t = tempfile::tempdir().unwrap();
    three_records(t.path());
    let log = std::fs::read_to_string(log_path(t.path(), 1)).unwrap();
    let lines: Vec<&str> = log.lines().collect();
    std::fs::write(log_path(t.path(), 1), format!("{}\n{}\n", lines[0], lines[2])).unwrap();
    let r = restore_one(t.path()).unwrap();
    assert_eq!((r.text.as_str(), r.from.rev), ("a", 1));
}

#[test]
fn a_corrupt_meta_or_snap_is_quarantined_never_deleted() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path();
    let (a, b) = ("aaaaaaaaaaaaaaaa", "bbbbbbbbbbbbbbbb");
    {
        let mut w = writer(dir);
        for rid in [a, b] {
            w.handle(RecoveryMsg::Switch {
                rid: rid.into(),
                generation: 1,
                rev: 0,
                text: Arc::from("precious"),
                meta: Box::new(meta(rid)),
            });
        }
    }
    std::fs::write(dir.join(format!("{a}.meta.json")), b"{not json").unwrap();
    let snap = dir.join(format!("{b}.1.snap"));
    let mut bytes = std::fs::read(&snap).unwrap();
    *bytes.last_mut().unwrap() ^= 0x20; // body no longer matches its blake3
    std::fs::write(&snap, bytes).unwrap();

    let mut w = writer(dir);
    let out = recovery::restore(&mut w, "0000000b", CAPS);
    assert!(out.is_empty());
    assert_eq!(w.shared().stats.quarantined.load(Ordering::Acquire), 2);
    let q = dir.join(recovery::QUARANTINE_DIR);
    for rid in [a, b] {
        assert!(rid_files(dir, rid).is_empty(), "{rid} left the recovery dir");
        let moved = rid_files(&q, rid);
        assert_eq!(moved.len(), 3, "{rid}: every file kept in quarantine: {moved:?}");
    }
}

#[test]
fn restore_sweeps_orphans() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path();
    {
        let mut w = writer(dir);
        w.handle(switch(1, 0, "keep"));
    }
    let orphan = "cccccccccccccccc";
    // A cleanup interrupted after its meta unlink; a non-current generation;
    // unfinished temp files.
    std::fs::write(dir.join(format!("{orphan}.4.snap")), "x").unwrap();
    std::fs::write(dir.join(format!("{orphan}.4.log")), "").unwrap();
    std::fs::write(dir.join(format!("{RID}.7.snap")), "stale").unwrap();
    std::fs::write(dir.join(format!("{RID}.8.snap.tmp")), "tmp").unwrap();
    std::fs::write(dir.join(format!("{RID}.meta.json.tmp")), "tmp").unwrap();
    let r = restore_one(dir).unwrap();
    assert_eq!(r.text, "keep");
    assert!(rid_files(dir, orphan).is_empty());
    let left = rid_files(dir, RID);
    assert_eq!(left, vec![format!("{RID}.2.log"), format!("{RID}.2.snap"), format!("{RID}.meta.json")], "only the new generation");
}

#[test]
fn a_switch_after_failed_ones_retires_every_older_generation() {
    // Opus m8: a switch whose step-5 directory fsync fails has already made
    // its generation current; one failing at step 3 never did. The next
    // durable switch must retire both, and the one before them.
    for failing in [3u8, 5] {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path();
        let mut w = writer(dir);
        w.handle(switch(1, 0, "base"));
        w.set_io_fault(move |step| step == failing);
        w.handle(switch(2, 1, "base+1"));
        assert!(!w.crashed());
        assert!(w.shared().is_failed(RID), "step {failing}: the failed switch degrades the rid");
        w.set_io_fault(|_| false);
        w.handle(switch(3, 2, "base+1+2"));
        let left = rid_files(dir, RID);
        assert_eq!(
            left,
            vec![format!("{RID}.3.log"), format!("{RID}.3.snap"), format!("{RID}.meta.json")],
            "step {failing}: only the new generation is left"
        );
        assert_eq!(meta_of(dir, RID).generation, 3);
        let r = restore_one(dir).unwrap();
        assert_eq!(r.text, "base+1+2");
    }
}

#[test]
fn a_failed_restore_switch_keeps_the_old_generation() {
    let t = tempfile::tempdir().unwrap();
    {
        let mut w = writer(t.path());
        w.handle(switch(1, 0, "base"));
        w.handle(append(1, 1, "base", "!"));
    }
    // The restore switch dies after step 3: the meta still names gen 1.
    let mut w = writer(t.path());
    w.set_fault(|f| f == Fault::SwitchStep(3));
    let r = recovery::restore(&mut w, "0000000b", CAPS).pop().unwrap();
    assert_eq!(r.text, "base!");
    assert!(r.needs_switch, "the actor must switch before anything else is written");
    let r = restore_one(t.path()).unwrap();
    assert_eq!((r.text.as_str(), r.from.rev), ("base!", 1));
}
