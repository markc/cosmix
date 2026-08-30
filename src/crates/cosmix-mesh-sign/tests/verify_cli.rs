use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use cosmix_mesh_trust::inventory::{
    ALG_ED25519, CANONICAL_ENCODING_V1, InvSignature, InventoryPayload, KeyStatus, SignedInventory,
    VerifyKey,
};
use ed25519_dalek::{Signer as _, SigningKey};
use serde_json::{Value, json};

static NEXT_TEMP: AtomicUsize = AtomicUsize::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "cosmix-mesh-sign-{tag}-{}-{sequence}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Fixture {
    _dir: TempDir,
    signed: PathBuf,
    genesis_pub: PathBuf,
    wrong_pub: PathBuf,
    authored: PathBuf,
    changed_authored: PathBuf,
    pubkey_b64: String,
}

impl Fixture {
    fn new() -> Self {
        Self::with_recovery_generation(None)
    }

    fn with_recovery_generation(recovery_generation: Option<u64>) -> Self {
        let dir = TempDir::new("verify");
        let signing_key = SigningKey::from_bytes(&[7u8; 32]);
        let pubkey_b64 = B64.encode(signing_key.verifying_key().to_bytes());
        let payload = InventoryPayload {
            schema_version: 1,
            canonical_encoding: CANONICAL_ENCODING_V1.into(),
            mesh: "example.internal".into(),
            subnet: "192.0.2.0/24".into(),
            epoch: 7,
            signed_at: "2026-06-03T00:00:00Z".into(),
            valid_until: "2026-09-01T00:00:00Z".into(),
            hub: vec!["beta".into()],
            verify_keys: vec![VerifyKey {
                key_id: "genesis".into(),
                pubkey: pubkey_b64.clone(),
                key_type: ALG_ED25519.into(),
                status: KeyStatus::Active,
            }],
            members: json!([{
                "name": "alpha",
                "mesh_ip": "192.0.2.5",
                "bus": true,
                "noded_port": 4300,
                "status": "active",
                "credentials": [],
                "last_touched_epoch": 7,
            }]),
            recovery: None,
            recovery_generation,
        };
        let signature = signing_key.sign(&payload.canonical_bytes());
        let envelope = SignedInventory {
            signatures: vec![InvSignature {
                key_id: "genesis".into(),
                alg: ALG_ED25519.into(),
                sig: B64.encode(signature.to_bytes()),
            }],
            payload,
        };

        let signed = dir.path().join("inventory.signed");
        let genesis_pub = dir.path().join("genesis.pub");
        let wrong_pub = dir.path().join("wrong.pub");
        let authored = dir.path().join("inventory.mix");
        let changed_authored = dir.path().join("changed.mix");
        std::fs::write(&signed, serde_json::to_vec_pretty(&envelope).unwrap()).unwrap();
        std::fs::write(&genesis_pub, format!("{pubkey_b64}\n")).unwrap();
        std::fs::write(&wrong_pub, format!("{}\n", B64.encode([8u8; 32]))).unwrap();
        std::fs::write(&authored, authored_text("192.0.2.5")).unwrap();
        std::fs::write(&changed_authored, authored_text("192.0.2.6")).unwrap();

        Self {
            _dir: dir,
            signed,
            genesis_pub,
            wrong_pub,
            authored,
            changed_authored,
            pubkey_b64,
        }
    }
}

fn authored_text(mesh_ip: &str) -> String {
    format!(
        concat!(
            "inventory: {{\n",
            "  schema_version: 1,\n",
            "  mesh: \"example.internal\",\n",
            "  subnet: \"192.0.2.0/24\",\n",
            "  epoch: 7,\n",
            "  hub: [\"beta\"],\n",
            "  members: [{{\n",
            "    name: \"alpha\", mesh_ip: \"{}\", bus: true, noded_port: 4300, status: \"active\",\n",
            "    credentials: [], last_touched_epoch: 7\n",
            "  }}],\n",
            "  unsigned: true\n",
            "}}\n"
        ),
        mesh_ip
    )
}

fn run(args: &[String]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cosmix-mesh-sign"))
        .args(args)
        .output()
        .unwrap()
}

fn strings(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_owned()).collect()
}

fn json_stdout(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout was not exactly one JSON value ({error}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

#[test]
fn genesis_pub_wrong_key_fails_without_opening_secrets_db() {
    let fixture = Fixture::new();
    let output = run(&strings(&[
        "--secrets-db",
        "/definitely/not/a/secrets.db",
        "verify",
        fixture.signed.to_str().unwrap(),
        "--genesis-pub",
        fixture.wrong_pub.to_str().unwrap(),
        "--json",
    ]));
    assert!(!output.status.success());
    let report = json_stdout(&output);
    assert_eq!(report["ok"], false);
    assert_eq!(report["format"], "cosmix-mesh-sign.verify.v1");
    assert_eq!(report["error"]["code"], "verification_failed");
}

#[test]
fn expected_mesh_mismatch_fails_after_authentication() {
    let fixture = Fixture::new();
    let output = run(&strings(&[
        "--secrets-db",
        "/definitely/not/a/secrets.db",
        "verify",
        fixture.signed.to_str().unwrap(),
        "--genesis-pub-base64",
        &fixture.pubkey_b64,
        "--expected-mesh",
        "other.internal",
        "--json",
    ]));
    assert!(!output.status.success());
    let report = json_stdout(&output);
    assert_eq!(report["error"]["code"], "mesh_mismatch");
    assert_eq!(report["details"]["actual_mesh"], "example.internal");
    assert_eq!(report["details"]["expected_mesh"], "other.internal");
}

#[test]
fn against_authored_matches_and_include_payload_is_authenticated_output() {
    let fixture = Fixture::new();
    let output = run(&strings(&[
        "--secrets-db",
        "/definitely/not/a/secrets.db",
        "verify",
        fixture.signed.to_str().unwrap(),
        "--genesis-pub",
        fixture.genesis_pub.to_str().unwrap(),
        "--expected-mesh",
        "example.internal",
        "--against-authored",
        fixture.authored.to_str().unwrap(),
        "--json",
        "--include-payload",
    ]));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let report = json_stdout(&output);
    assert_eq!(report["ok"], true);
    assert_eq!(report["member_count"], 1);
    assert_eq!(report["mesh"], "example.internal");
    assert_eq!(report["routing_view"]["valid"], true);
    assert_eq!(report["routing_view"]["members"][0]["class"], "active-bus");
    assert_eq!(report["routing_view"]["members"][0]["mesh_ip"], "192.0.2.5");
    assert_eq!(report["routing_view"]["members"][0]["noded_port"], 4300);
    assert_eq!(report["against_authored"]["matches"], true);
    assert_eq!(
        report["against_authored"]["signed_authoring_blake3"],
        report["against_authored"]["authored_authoring_blake3"]
    );
    assert_eq!(
        report["authoring_blake3"],
        report["against_authored"]["signed_authoring_blake3"]
    );
    assert_eq!(report["payload"]["members"][0]["mesh_ip"], "192.0.2.5");
    assert_eq!(report["recovery_generation"], 0);
    assert_eq!(report["payload_recovery_generation"], Value::Null);
}

#[test]
fn verify_reports_payload_recovery_generation_when_present() {
    let fixture = Fixture::with_recovery_generation(Some(0));
    let output = run(&strings(&[
        "verify",
        fixture.signed.to_str().unwrap(),
        "--genesis-pub",
        fixture.genesis_pub.to_str().unwrap(),
        "--json",
    ]));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = json_stdout(&output);
    assert_eq!(report["recovery_generation"], 0);
    assert_eq!(report["payload_recovery_generation"], 0);
}

#[test]
fn human_verify_reports_effective_and_payload_generation() {
    let fixture = Fixture::new();
    let output = run(&strings(&[
        "verify",
        fixture.signed.to_str().unwrap(),
        "--genesis-pub",
        fixture.genesis_pub.to_str().unwrap(),
    ]));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("recovery_generation=0"), "{stdout}");
    assert!(
        stdout.contains("payload_recovery_generation=null"),
        "{stdout}"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("legacy generation-silent payload"),
        "{stderr}"
    );
    assert!(
        stderr.contains("synthetic recovery-generation floor of 0"),
        "{stderr}"
    );
    assert!(stderr.contains("cached generation exceeds 0"), "{stderr}");
}

#[test]
fn against_authored_fails_when_a_member_field_changes() {
    let fixture = Fixture::new();
    let output = run(&strings(&[
        "verify",
        fixture.signed.to_str().unwrap(),
        "--genesis-pub",
        fixture.genesis_pub.to_str().unwrap(),
        "--against-authored",
        fixture.changed_authored.to_str().unwrap(),
        "--json",
    ]));
    assert!(!output.status.success());
    let report = json_stdout(&output);
    assert_eq!(report["error"]["code"], "authoring_mismatch");
    assert_eq!(report["details"]["against_authored"]["matches"], false);
    assert_ne!(
        report["details"]["against_authored"]["signed_authoring_blake3"],
        report["details"]["against_authored"]["authored_authoring_blake3"]
    );
}
