//! `maild.dkim.*` Bus action handlers — per-domain DKIM key lifecycle.
//!
//! Phase 4 ships three verbs:
//!   * `maild.dkim.generate` — fresh keypair, write PEM atomically,
//!     stamp the selector into the `maild.domains` row, rebuild and
//!     swap the in-memory signer. First generate per domain promotes
//!     itself to active; subsequent generates remain inactive until
//!     explicitly rotated.
//!   * `maild.dkim.rotate` — promote an already-present selector to
//!     `dkim_selector_active`. The PEM must already be on disk (no
//!     file-system side-effect on this verb).
//!   * `maild.dkim.retire` — remove a non-active selector from
//!     `dkim_selectors`. Swap the signer first, THEN unlink the PEM so
//!     a concurrent sign call cannot race with file removal.
//!
//! Every verb refuses to touch a domain that has an operator-managed
//! `[[dkim.domain]]` baseline row (the same "pick one source of truth"
//! gate the startup rebuild enforces).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use arc_swap::ArcSwap;
use cosmix_client::IncomingCommand;
use cosmix_maild_auth::{
    DkimAlgorithm, DkimSignerConfig, Domain, MailAuthSigner,
    keygen::{self, KeySpec},
};
use cosmix_props::MergeMode;
use cosmix_props::record::{Actor, RecordKey};
use cosmix_props::runtime::{Runtime, SetOpts};
use cosmix_props::value::PropValue;
use serde_json::{Value as JsonValue, json};
use tokio::sync::Mutex;

use crate::dkim_rebuild;
use crate::props::domains;

const RC_OK: u8 = 0;
const RC_ERROR: u8 = 10;

/// Shared state threaded through `bus::run` for the DKIM verb surface.
/// Clone-cheap (every field is already `Arc`-shaped).
#[derive(Clone)]
pub struct DkimState {
    /// The live signer slot read by the SMTP submission path. Replaced
    /// atomically once each verb completes its substrate + filesystem
    /// transition.
    pub signer_slot: Arc<ArcSwap<Option<Arc<MailAuthSigner>>>>,
    /// Operator-configured `[[dkim.domain]]` rows captured at startup.
    /// Verbs read this snapshot to enforce the baseline-conflict gate
    /// — the configs themselves never change at runtime, only the
    /// substrate side does.
    pub baseline: Arc<Vec<DkimSignerConfig>>,
    /// The `maild.domains` runtime for substrate reads/writes.
    pub domains_runtime: Arc<Runtime>,
    /// On-disk root: `<key_root>/<domain>/<selector>.pem`.
    pub key_root: PathBuf,
    /// Per-domain serialisation so concurrent verbs against the same
    /// domain see a consistent (read-modify-write, rebuild, swap)
    /// transition. The outer `Mutex<HashMap>` guards the map; the
    /// inner `Arc<Mutex<()>>` is per-domain.
    pub locks: Arc<Mutex<HashMap<Domain, Arc<Mutex<()>>>>>,
}

impl DkimState {
    pub fn new(
        signer_slot: Arc<ArcSwap<Option<Arc<MailAuthSigner>>>>,
        baseline: Arc<Vec<DkimSignerConfig>>,
        domains_runtime: Arc<Runtime>,
        key_root: PathBuf,
    ) -> Self {
        Self {
            signer_slot,
            baseline,
            domains_runtime,
            key_root,
            locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Acquire (or create) the per-domain serialisation lock. The
    /// returned guard must be held for the entire (read-substrate,
    /// write-pem, write-substrate, rebuild, swap) transition.
    async fn lock_for(&self, domain: &Domain) -> Arc<Mutex<()>> {
        let mut map = self.locks.lock().await;
        map.entry(domain.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
}

/// Dispatch a `maild.dkim.*` action. Returns `(rc, body_json)`.
pub async fn dispatch(action: &str, cmd: &IncomingCommand, state: &DkimState) -> (u8, String) {
    match action {
        "generate" => to_response(handle_generate(cmd, state).await),
        "rotate" => to_response(handle_rotate(cmd, state).await),
        "retire" => to_response(handle_retire(cmd, state).await),
        other => (RC_ERROR, err_body(&format!("unknown dkim action: {other}"))),
    }
}

/// Build a substrate audit-cause string that records both the verb and
/// the Bus caller (`cmd.from`). The wire-level trust posture is
/// WG-only mesh (see `feedback_bus_wire_trust_boundary`), so audit IS
/// the accountability layer here — the cause must name who triggered
/// the mutation, not just what verb fired it.
fn audit_cause(action: &str, cmd: &IncomingCommand) -> String {
    if cmd.from.is_empty() {
        format!("maild.dkim.{action}")
    } else {
        format!("maild.dkim.{action} (peer={})", cmd.from)
    }
}

fn to_response(r: Result<JsonValue>) -> (u8, String) {
    match r {
        Ok(v) => (RC_OK, v.to_string()),
        Err(e) => (RC_ERROR, err_body(&e.to_string())),
    }
}

fn err_body(msg: &str) -> String {
    json!({ "error": msg }).to_string()
}

#[derive(Debug)]
struct VerbArgs {
    domain: String,
    selector: String,
    algorithm: Option<DkimAlgorithm>,
}

fn parse_args(cmd: &IncomingCommand, want_algorithm: bool) -> Result<VerbArgs> {
    let args = super::resolve_args(cmd);
    let obj = args
        .as_object()
        .ok_or_else(|| anyhow!("expected args object with `domain` and `selector` fields"))?;
    let domain = obj
        .get("domain")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing or non-string `domain`"))?
        .to_ascii_lowercase();
    domains::validate_fqdn(&domain).map_err(|e| anyhow!("invalid domain {domain:?}: {e}"))?;
    let selector = obj
        .get("selector")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing or non-string `selector`"))?
        .to_string();
    domains::validate_dkim_selector(&selector)
        .map_err(|e| anyhow!("invalid selector {selector:?}: {e}"))?;
    let algorithm = if want_algorithm {
        let raw = obj
            .get("algorithm")
            .and_then(|v| v.as_str())
            .unwrap_or("rsa-sha256");
        Some(match raw {
            "rsa-sha256" => DkimAlgorithm::RsaSha256,
            "ed25519-sha256" => DkimAlgorithm::Ed25519Sha256,
            other => {
                bail!("unknown algorithm {other:?} (want \"rsa-sha256\" or \"ed25519-sha256\")")
            }
        })
    } else {
        None
    };
    Ok(VerbArgs {
        domain,
        selector,
        algorithm,
    })
}

/// Reject the verb early if `domain` is operator-managed via
/// `[[dkim.domain]]`. Mirrors the startup rebuild's "pick one source
/// of truth" rule.
fn ensure_not_baseline(state: &DkimState, domain: &str) -> Result<()> {
    if state.baseline.iter().any(|c| c.domain.as_str() == domain) {
        bail!(
            "domain {:?} is operator-managed via [[dkim.domain]]; \
             substrate verbs refuse to touch it",
            domain
        );
    }
    Ok(())
}

/// Read the current `maild.domains` row (Replace-mode upsert needs the
/// existing object verbatim so unrelated fields survive). Returns the
/// row's defaults if absent — operators expect to be able to `generate`
/// on a fresh domain without a prior `maild.domains.set`.
async fn read_or_default_row(
    domains_runtime: &Arc<Runtime>,
    domain: &str,
) -> Result<(PropValue, cosmix_props::record::Version)> {
    let key = RecordKey::collection(domains::namespace_name(), domain.to_string());
    match domains_runtime.store().get(&key).await {
        Ok(snap) => Ok((snap.value.value, snap.value.version)),
        Err(cosmix_props::StoreError::NotFound) => Ok((
            domains::defaults_value(domain),
            cosmix_props::record::Version::zero(),
        )),
        Err(e) => Err(anyhow!("read maild.domains/{domain}: {e}")),
    }
}

/// Commit `body` back with `expected_version=version` (Replace mode).
async fn write_row(
    domains_runtime: &Arc<Runtime>,
    domain: &str,
    body: PropValue,
    version: cosmix_props::record::Version,
    cause: &str,
) -> Result<()> {
    let key = RecordKey::collection(domains::namespace_name(), domain.to_string());
    domains_runtime
        .set(
            key,
            body,
            SetOpts {
                expected_version: Some(version),
                merge: MergeMode::Replace,
                actor: Actor::service("maild.dkim"),
                cause: Some(cause.to_string()),
                ts_ms: chrono::Utc::now().timestamp_millis(),
            },
        )
        .await
        .with_context(|| format!("set maild.domains/{domain}"))?;
    Ok(())
}

/// Rebuild signer from (baseline + substrate) and atomically swap.
async fn rebuild_and_swap(state: &DkimState) -> Result<()> {
    let built = dkim_rebuild::rebuild_signer_from_substrate(
        &state.baseline,
        &state.domains_runtime,
        &state.key_root,
    )
    .await
    .context("rebuild_signer_from_substrate after dkim verb")?;
    state.signer_slot.store(Arc::new(built));
    Ok(())
}

fn list_get<'a>(body: &'a PropValue, field: &str) -> &'a [PropValue] {
    match body {
        PropValue::Object(m) => match m.get(field) {
            Some(PropValue::List(v)) => v.as_slice(),
            _ => &[],
        },
        _ => &[],
    }
}

fn opt_string_get(body: &PropValue, field: &str) -> Option<String> {
    match body {
        PropValue::Object(m) => match m.get(field) {
            Some(PropValue::String(s)) => Some(s.clone()),
            _ => None,
        },
        _ => None,
    }
}

fn obj_mut(body: &mut PropValue) -> Result<&mut std::collections::BTreeMap<String, PropValue>> {
    match body {
        PropValue::Object(m) => Ok(m),
        _ => Err(anyhow!("maild.domains row body is not an Object")),
    }
}

// ---- generate ----

async fn handle_generate(cmd: &IncomingCommand, state: &DkimState) -> Result<JsonValue> {
    let args = parse_args(cmd, true)?;
    ensure_not_baseline(state, &args.domain)?;

    let dom = Domain::new(args.domain.clone());
    let lock = state.lock_for(&dom).await;
    let _guard = lock.lock().await;

    // 1. Read row.
    let (mut body, version) = read_or_default_row(&state.domains_runtime, &args.domain).await?;

    // 2. Selector already present → caller error (idempotency would
    // hide a key replacement bug if we silently no-op'd here; force
    // an explicit `retire` first).
    let already_listed = list_get(&body, "dkim_selectors")
        .iter()
        .filter_map(|v| match v {
            PropValue::String(s) => Some(s.as_str()),
            _ => None,
        })
        .any(|s| s == args.selector.as_str());
    if already_listed {
        bail!(
            "selector {:?} already present on domain {:?}; retire it before regenerating",
            args.selector,
            args.domain
        );
    }

    // 3. Generate keypair.
    let algorithm = args.algorithm.expect("parse_args set algorithm");
    let key_spec = match algorithm {
        DkimAlgorithm::RsaSha256 => KeySpec::Rsa { bits: 2048 },
        DkimAlgorithm::Ed25519Sha256 => KeySpec::Ed25519,
    };
    let material = keygen::generate(key_spec).context("generate keypair")?;

    // 4. Atomic O_EXCL write at `<key_root>/<domain>/<selector>.pem`
    // with mode 0600. Refuses to clobber a pre-existing file — that
    // would indicate selector collision the substrate row didn't
    // record (operator hand-edit, prior crash mid-write, etc) and
    // forcing them to investigate is safer than silent overwrite.
    let pem_path = dkim_rebuild::pem_path(&state.key_root, &args.domain, &args.selector);
    if let Some(parent) = pem_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("mkdir -p {}", parent.display()))?;
    }
    write_pem_atomic(&pem_path, material.private_pem.as_bytes())
        .with_context(|| format!("write PEM {}", pem_path.display()))?;

    // 5. Build the new substrate body — add selector to
    // `dkim_selectors`, set active to this selector only if no row
    // had one before. Operators rotate explicitly with the rotate
    // verb, so a stable existing-active is not silently displaced.
    let promote_to_active = opt_string_get(&body, "dkim_selector_active").is_none();
    {
        let obj = obj_mut(&mut body)?;
        let mut selectors: Vec<PropValue> = match obj.remove("dkim_selectors") {
            Some(PropValue::List(v)) => v,
            _ => Vec::new(),
        };
        selectors.push(PropValue::String(args.selector.clone()));
        obj.insert("dkim_selectors".into(), PropValue::List(selectors));
        if promote_to_active {
            obj.insert(
                "dkim_selector_active".into(),
                PropValue::String(args.selector.clone()),
            );
        }
    }

    // 6. Write substrate row. If this fails, unlink the PEM so we
    // don't leave an orphan key on disk.
    let cause = audit_cause("generate", cmd);
    if let Err(e) = write_row(&state.domains_runtime, &args.domain, body, version, &cause).await {
        let _ = std::fs::remove_file(&pem_path);
        return Err(e);
    }

    // 7. Rebuild signer + swap.
    rebuild_and_swap(state).await?;

    Ok(json!({
        "domain": args.domain,
        "selector": args.selector,
        "algorithm": match algorithm {
            DkimAlgorithm::RsaSha256 => "rsa-sha256",
            DkimAlgorithm::Ed25519Sha256 => "ed25519-sha256",
        },
        "active": promote_to_active,
        "dns_txt": material.dns_txt_record,
        "public_b64": material.public_b64,
    }))
}

/// Atomic, no-clobber write with mode 0600. `O_EXCL` makes the create
/// refuse if the file already exists; verifies the permissions are
/// what we asked for before claiming success (a permissive umask is
/// not allowed to silently widen the key file). Any failure AFTER the
/// `create_new` succeeds unlinks the partially-written file before
/// returning so that retries don't get stuck on `EEXIST` and so
/// half-written / mis-permissioned PEMs never persist. Failures from
/// the `create_new` step itself (notably `AlreadyExists`) do NOT
/// unlink — the no-clobber property must hold for pre-existing files
/// the caller never owned. Also fsyncs the parent directory so the
/// new directory entry is durable before the substrate row that names
/// this PEM commits — without that, a crash between the PEM-write and
/// the substrate-commit could leave a durable substrate row pointing
/// at a non-existent PEM, which is a startup-fatal condition (active
/// selector missing PEM).
fn write_pem_atomic(path: &std::path::Path, content: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    // Phase 1: create exclusively. Failures here (including
    // `AlreadyExists`) propagate WITHOUT touching the filesystem —
    // we never owned the path.
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;

    // Phase 2: everything below this point operates on a file we
    // created. Any error must remove the partial file before
    // returning so a retry doesn't get stuck on `EEXIST` and no
    // half-written PEM persists. Helper closures cannot early-return
    // through `?`, so wrap the post-create work in an inline IIFE.
    let post_create = || -> std::io::Result<()> {
        f.write_all(content)?;
        f.sync_all()?;
        drop(f);
        let mode = std::fs::metadata(path)?.permissions().mode();
        if (mode & 0o777) != 0o600 {
            // Tighten if the platform widened (e.g. ACL inheritance,
            // unusual mount). Best-effort: a stricter check would
            // refuse, but DKIM PEMs are tolerant to the daemon's own
            // writes — re-set then re-verify.
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
            let mode2 = std::fs::metadata(path)?.permissions().mode();
            if (mode2 & 0o777) != 0o600 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!("PEM mode {mode2:o} != 0600 after chmod"),
                ));
            }
        }
        // Parent directory fsync — without this, the directory entry
        // for the new PEM is not guaranteed durable, and a crash
        // before the substrate row commits could leave the row
        // pointing at a non-existent PEM. The substrate's startup
        // rebuild treats a missing active-selector PEM as fatal, so
        // this is load-bearing.
        if let Some(parent) = path.parent() {
            let dir = std::fs::File::open(parent)?;
            dir.sync_all()?;
        }
        Ok(())
    };
    let result = post_create();
    if result.is_err() {
        // Created-but-incomplete cleanup. The `AlreadyExists` path
        // above already returned without entering this branch, so
        // an unlink here only ever targets a file THIS call created.
        let _ = std::fs::remove_file(path);
    }
    result
}

// ---- rotate ----

async fn handle_rotate(cmd: &IncomingCommand, state: &DkimState) -> Result<JsonValue> {
    let args = parse_args(cmd, false)?;
    ensure_not_baseline(state, &args.domain)?;

    let dom = Domain::new(args.domain.clone());
    let lock = state.lock_for(&dom).await;
    let _guard = lock.lock().await;

    let (mut body, version) = read_or_default_row(&state.domains_runtime, &args.domain).await?;

    // Selector must already be present in dkim_selectors.
    let listed = list_get(&body, "dkim_selectors")
        .iter()
        .filter_map(|v| match v {
            PropValue::String(s) => Some(s.as_str()),
            _ => None,
        })
        .any(|s| s == args.selector.as_str());
    if !listed {
        bail!(
            "selector {:?} not present on domain {:?}; generate it first",
            args.selector,
            args.domain
        );
    }

    let already_active =
        opt_string_get(&body, "dkim_selector_active").as_deref() == Some(args.selector.as_str());
    if !already_active {
        // PEM for the target selector must be on disk before we promote
        // it — a missing PEM here would convert into a startup-fatal
        // condition on next boot (active selector missing PEM).
        let pem_path = dkim_rebuild::pem_path(&state.key_root, &args.domain, &args.selector);
        if !pem_path.exists() {
            bail!(
                "selector {:?} listed for domain {:?} but PEM missing at {} — \
                 regenerate before rotating",
                args.selector,
                args.domain,
                pem_path.display()
            );
        }

        let obj = obj_mut(&mut body)?;
        obj.insert(
            "dkim_selector_active".into(),
            PropValue::String(args.selector.clone()),
        );

        write_row(
            &state.domains_runtime,
            &args.domain,
            body,
            version,
            &audit_cause("rotate", cmd),
        )
        .await?;
    }

    // Even on the idempotent fast-path, rebuild + swap so a previously
    // partial state (signer slot diverged from substrate) heals.
    rebuild_and_swap(state).await?;

    Ok(json!({
        "domain": args.domain,
        "active": args.selector,
        "changed": !already_active,
    }))
}

// ---- retire ----

async fn handle_retire(cmd: &IncomingCommand, state: &DkimState) -> Result<JsonValue> {
    let args = parse_args(cmd, false)?;
    ensure_not_baseline(state, &args.domain)?;

    let dom = Domain::new(args.domain.clone());
    let lock = state.lock_for(&dom).await;
    let _guard = lock.lock().await;

    let (mut body, version) = read_or_default_row(&state.domains_runtime, &args.domain).await?;

    let listed = list_get(&body, "dkim_selectors")
        .iter()
        .filter_map(|v| match v {
            PropValue::String(s) => Some(s.as_str()),
            _ => None,
        })
        .any(|s| s == args.selector.as_str());
    if !listed {
        bail!(
            "selector {:?} not present on domain {:?}",
            args.selector,
            args.domain
        );
    }
    if opt_string_get(&body, "dkim_selector_active").as_deref() == Some(args.selector.as_str()) {
        bail!(
            "selector {:?} is currently active on domain {:?}; rotate to a different \
             active selector before retiring",
            args.selector,
            args.domain
        );
    }

    // Remove from dkim_selectors.
    {
        let obj = obj_mut(&mut body)?;
        let selectors: Vec<PropValue> = match obj.remove("dkim_selectors") {
            Some(PropValue::List(v)) => v
                .into_iter()
                .filter(|v| match v {
                    PropValue::String(s) => s != args.selector.as_str(),
                    _ => true,
                })
                .collect(),
            _ => Vec::new(),
        };
        obj.insert("dkim_selectors".into(), PropValue::List(selectors));
    }

    write_row(
        &state.domains_runtime,
        &args.domain,
        body,
        version,
        &audit_cause("retire", cmd),
    )
    .await?;

    // Pre-swap-before-unlink: build the new signer (which no longer
    // references this selector), swap it into the slot, THEN remove
    // the PEM. A concurrent sign call holding the prior signer Arc
    // still has the in-memory key; the load_full() on the next call
    // sees the new signer.
    rebuild_and_swap(state).await?;

    let pem_path = dkim_rebuild::pem_path(&state.key_root, &args.domain, &args.selector);
    let unlink_err = std::fs::remove_file(&pem_path)
        .err()
        .filter(|e| e.kind() != std::io::ErrorKind::NotFound);
    if let Some(e) = unlink_err {
        // Substrate already moved on; surface the unlink failure as a
        // warning, not a hard error — the daemon's behaviour is
        // already correct (signer no longer references the file).
        tracing::warn!(
            domain = %args.domain,
            selector = %args.selector,
            path = %pem_path.display(),
            error = %e,
            "retire: failed to remove retired PEM (substrate already updated)"
        );
    }

    Ok(json!({
        "domain": args.domain,
        "retired": args.selector,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_client::IncomingCommand;
    use cosmix_props::bus::mutation::PropsRouter;
    use cosmix_props::record::Version;
    use cosmix_props::sqlite::SqliteStore;
    use rusqlite::Connection;
    use std::sync::Arc;

    fn fresh_state() -> (DkimState, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let conn = Connection::open_in_memory().expect("in-memory sqlite");
        let store = Arc::new(SqliteStore::new("maild", conn).expect("store"));
        let mut router = PropsRouter::new("maild");
        let runtime = domains::register(&mut router, &store).expect("register");
        let signer_slot = Arc::new(ArcSwap::from_pointee(None));
        let baseline = Arc::new(Vec::new());
        let state = DkimState::new(signer_slot, baseline, runtime, tmp.path().to_path_buf());
        (state, tmp)
    }

    fn cmd_with_args(action: &str, args: JsonValue) -> IncomingCommand {
        let mut headers = std::collections::BTreeMap::new();
        headers.insert("args".to_string(), args.to_string());
        IncomingCommand {
            command: format!("maild.dkim.{action}"),
            from: String::new(),
            id: None,
            args: JsonValue::Null,
            headers,
            body: String::new(),
        }
    }

    fn parse_response(rc: u8, body: String) -> JsonValue {
        assert_eq!(rc, RC_OK, "verb returned rc={rc} body={body}");
        serde_json::from_str(&body).expect("response is JSON")
    }

    // ---- Bucket C: per-verb correctness ----

    #[tokio::test]
    async fn generate_creates_pem_and_marks_active() {
        let (state, _tmp) = fresh_state();
        let cmd = cmd_with_args(
            "generate",
            json!({
                "domain": "k1.example",
                "selector": "s2026a",
                "algorithm": "ed25519-sha256",
            }),
        );
        let (rc, body) = dispatch("generate", &cmd, &state).await;
        let v = parse_response(rc, body);
        assert_eq!(v["selector"], "s2026a");
        assert_eq!(v["algorithm"], "ed25519-sha256");
        assert_eq!(v["active"], true);
        assert!(v["dns_txt"].as_str().unwrap().starts_with("v=DKIM1"));
        // PEM exists with mode 0600.
        let pem = dkim_rebuild::pem_path(&state.key_root, "k1.example", "s2026a");
        let meta = std::fs::metadata(&pem).expect("pem present");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        // Substrate row: selector listed, active set.
        let (body, _v) = read_or_default_row(&state.domains_runtime, "k1.example")
            .await
            .unwrap();
        let sels: Vec<String> = list_get(&body, "dkim_selectors")
            .iter()
            .filter_map(|v| match v {
                PropValue::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(sels, vec!["s2026a"]);
        assert_eq!(
            opt_string_get(&body, "dkim_selector_active").as_deref(),
            Some("s2026a")
        );
        // Signer slot populated.
        let snap = state.signer_slot.load_full();
        let signer = snap.as_ref().as_ref().expect("signer present");
        signer
            .lookup_active(&Domain::new("k1.example"))
            .expect("active resolvable");
    }

    #[tokio::test]
    async fn generate_second_does_not_displace_active() {
        let (state, _tmp) = fresh_state();
        for sel in ["a", "b"] {
            let cmd = cmd_with_args(
                "generate",
                json!({
                    "domain": "k2.example",
                    "selector": sel,
                    "algorithm": "rsa-sha256",
                }),
            );
            let (rc, body) = dispatch("generate", &cmd, &state).await;
            assert_eq!(rc, RC_OK, "{body}");
        }
        let (body, _v) = read_or_default_row(&state.domains_runtime, "k2.example")
            .await
            .unwrap();
        let sels: Vec<String> = list_get(&body, "dkim_selectors")
            .iter()
            .filter_map(|v| match v {
                PropValue::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(sels, vec!["a", "b"]);
        assert_eq!(
            opt_string_get(&body, "dkim_selector_active").as_deref(),
            Some("a")
        );
    }

    #[tokio::test]
    async fn generate_refuses_duplicate_selector() {
        let (state, _tmp) = fresh_state();
        let cmd = cmd_with_args(
            "generate",
            json!({"domain": "k3.example", "selector": "d", "algorithm": "rsa-sha256"}),
        );
        let (rc, _) = dispatch("generate", &cmd, &state).await;
        assert_eq!(rc, RC_OK);
        let (rc2, body2) = dispatch("generate", &cmd, &state).await;
        assert_eq!(rc2, RC_ERROR);
        assert!(body2.contains("already present"), "{body2}");
    }

    #[tokio::test]
    async fn rotate_promotes_existing_selector() {
        let (state, _tmp) = fresh_state();
        for sel in ["a", "b"] {
            let cmd = cmd_with_args(
                "generate",
                json!({"domain": "k4.example", "selector": sel, "algorithm": "rsa-sha256"}),
            );
            assert_eq!(dispatch("generate", &cmd, &state).await.0, RC_OK);
        }
        let cmd = cmd_with_args("rotate", json!({"domain": "k4.example", "selector": "b"}));
        let (rc, body) = dispatch("rotate", &cmd, &state).await;
        let v = parse_response(rc, body);
        assert_eq!(v["active"], "b");
        assert_eq!(v["changed"], true);
        let (body, _v) = read_or_default_row(&state.domains_runtime, "k4.example")
            .await
            .unwrap();
        assert_eq!(
            opt_string_get(&body, "dkim_selector_active").as_deref(),
            Some("b")
        );
    }

    #[tokio::test]
    async fn rotate_refuses_unknown_selector() {
        let (state, _tmp) = fresh_state();
        let cmd = cmd_with_args(
            "rotate",
            json!({"domain": "k5.example", "selector": "nope"}),
        );
        let (rc, body) = dispatch("rotate", &cmd, &state).await;
        assert_eq!(rc, RC_ERROR);
        assert!(body.contains("not present"), "{body}");
    }

    #[tokio::test]
    async fn retire_drops_selector_and_unlinks_pem() {
        let (state, _tmp) = fresh_state();
        for sel in ["a", "b"] {
            let cmd = cmd_with_args(
                "generate",
                json!({"domain": "k6.example", "selector": sel, "algorithm": "rsa-sha256"}),
            );
            assert_eq!(dispatch("generate", &cmd, &state).await.0, RC_OK);
        }
        let pem_b = dkim_rebuild::pem_path(&state.key_root, "k6.example", "b");
        assert!(pem_b.exists());
        let cmd = cmd_with_args("retire", json!({"domain": "k6.example", "selector": "b"}));
        let (rc, body) = dispatch("retire", &cmd, &state).await;
        let _v = parse_response(rc, body);
        assert!(!pem_b.exists(), "PEM unlinked after retire");
        let (body, _v) = read_or_default_row(&state.domains_runtime, "k6.example")
            .await
            .unwrap();
        let sels: Vec<String> = list_get(&body, "dkim_selectors")
            .iter()
            .filter_map(|v| match v {
                PropValue::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(sels, vec!["a"]);
    }

    #[tokio::test]
    async fn retire_refuses_active_selector() {
        let (state, _tmp) = fresh_state();
        let cmd = cmd_with_args(
            "generate",
            json!({"domain": "k7.example", "selector": "only", "algorithm": "rsa-sha256"}),
        );
        assert_eq!(dispatch("generate", &cmd, &state).await.0, RC_OK);
        let cmd = cmd_with_args(
            "retire",
            json!({"domain": "k7.example", "selector": "only"}),
        );
        let (rc, body) = dispatch("retire", &cmd, &state).await;
        assert_eq!(rc, RC_ERROR);
        assert!(body.contains("currently active"), "{body}");
    }

    #[tokio::test]
    async fn baseline_conflict_rejected_on_every_verb() {
        let (mut state, _tmp) = fresh_state();
        // Inject a baseline row for k8.example so the baseline guard
        // fires.
        let baseline = Arc::new(vec![DkimSignerConfig {
            domain: Domain::new("k8.example".to_string()),
            selector: "config".into(),
            algorithm: DkimAlgorithm::RsaSha256,
            private_key_pem: Vec::new(),
            canonicalization: (
                cosmix_maild_auth::Canon::Relaxed,
                cosmix_maild_auth::Canon::Relaxed,
            ),
            headers_to_sign: Vec::new(),
            active_for_signing: true,
            allow_body_length_tag: false,
        }]);
        state.baseline = baseline;
        for verb in ["generate", "rotate", "retire"] {
            let args = if verb == "generate" {
                json!({"domain": "k8.example", "selector": "s", "algorithm": "rsa-sha256"})
            } else {
                json!({"domain": "k8.example", "selector": "s"})
            };
            let cmd = cmd_with_args(verb, args);
            let (rc, body) = dispatch(verb, &cmd, &state).await;
            assert_eq!(rc, RC_ERROR, "verb={verb} body={body}");
            assert!(
                body.contains("operator-managed via [[dkim.domain]]"),
                "verb={verb} body={body}"
            );
        }
    }

    // ---- Bucket E: live-swap determinism ----

    #[tokio::test]
    async fn each_verb_swaps_to_a_new_arc() {
        let (state, _tmp) = fresh_state();
        let cmd1 = cmd_with_args(
            "generate",
            json!({"domain": "k9.example", "selector": "a", "algorithm": "rsa-sha256"}),
        );
        assert_eq!(dispatch("generate", &cmd1, &state).await.0, RC_OK);
        let after_gen = state.signer_slot.load_full();
        let after_gen_arc = after_gen.as_ref().as_ref().expect("present").clone();

        let cmd2 = cmd_with_args(
            "generate",
            json!({"domain": "k9.example", "selector": "b", "algorithm": "rsa-sha256"}),
        );
        assert_eq!(dispatch("generate", &cmd2, &state).await.0, RC_OK);
        let after_gen2 = state.signer_slot.load_full();
        let after_gen2_arc = after_gen2.as_ref().as_ref().expect("present").clone();
        assert!(
            !Arc::ptr_eq(&after_gen_arc, &after_gen2_arc),
            "second generate must produce a new signer Arc"
        );

        let cmd3 = cmd_with_args("rotate", json!({"domain": "k9.example", "selector": "b"}));
        assert_eq!(dispatch("rotate", &cmd3, &state).await.0, RC_OK);
        let after_rot = state.signer_slot.load_full();
        let after_rot_arc = after_rot.as_ref().as_ref().expect("present").clone();
        assert!(
            !Arc::ptr_eq(&after_gen2_arc, &after_rot_arc),
            "rotate must produce a new signer Arc"
        );

        let cmd4 = cmd_with_args("retire", json!({"domain": "k9.example", "selector": "a"}));
        assert_eq!(dispatch("retire", &cmd4, &state).await.0, RC_OK);
        let after_ret = state.signer_slot.load_full();
        let after_ret_arc = after_ret.as_ref().as_ref().expect("present").clone();
        assert!(
            !Arc::ptr_eq(&after_rot_arc, &after_ret_arc),
            "retire must produce a new signer Arc"
        );
    }

    /// `Version::zero()` is the substrate's "absent or freshly
    /// created" sentinel — every commit_set bumps version, so two
    /// concurrent verbs both reading version=0 would collide on the
    /// second's commit. This test pins the optimistic-concurrency
    /// behaviour: the second verb in a race against itself loses with
    /// a VersionMismatch surfaced through the dispatch error path.
    #[tokio::test]
    async fn double_generate_serialised_by_per_domain_lock() {
        let (state, _tmp) = fresh_state();
        let state1 = state.clone();
        let state2 = state.clone();
        let cmd_a = cmd_with_args(
            "generate",
            json!({"domain": "race.example", "selector": "a", "algorithm": "rsa-sha256"}),
        );
        let cmd_b = cmd_with_args(
            "generate",
            json!({"domain": "race.example", "selector": "b", "algorithm": "rsa-sha256"}),
        );
        let h1 = tokio::spawn(async move { dispatch("generate", &cmd_a, &state1).await });
        let h2 = tokio::spawn(async move { dispatch("generate", &cmd_b, &state2).await });
        let (r1, r2) = tokio::join!(h1, h2);
        let (rc1, _) = r1.unwrap();
        let (rc2, _) = r2.unwrap();
        // Per-domain lock serialises them; both succeed because the
        // second reads the post-commit version after the first
        // releases the guard.
        assert_eq!(rc1, RC_OK);
        assert_eq!(rc2, RC_OK);
        // Both selectors present.
        let (body, _v) = read_or_default_row(&state.domains_runtime, "race.example")
            .await
            .unwrap();
        let sels: std::collections::BTreeSet<String> = list_get(&body, "dkim_selectors")
            .iter()
            .filter_map(|v| match v {
                PropValue::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert!(sels.contains("a") && sels.contains("b"), "got {sels:?}");
        // Verify Version is now > zero, so the lock was needed.
        let key = RecordKey::collection(domains::namespace_name(), "race.example".to_string());
        let snap = state.domains_runtime.store().get(&key).await.unwrap();
        assert!(snap.value.version > Version::zero());
    }
}
