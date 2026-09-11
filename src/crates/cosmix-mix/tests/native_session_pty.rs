//! S3: real noded + Term's real LaunchFd/PTY mapping + the built Mix binary.
//! The fixture owns the Term-side record/renew/re-grant/revoke duties. Term's
//! GUI mutation and exit-notifier ordering remain covered in its own workspace.
#![cfg(target_os = "linux")]

// Term's embedded source uses these crate aliases; the Mix manifest explicitly
// names the same dependencies cosmix_lib_bus / cosmix_lib_client.
extern crate cosmix_lib_bus as cosmix_bus;
extern crate cosmix_lib_client as cosmix_client;

#[allow(dead_code)] // quarantine is Term-only; embedded tests exercise it too.
#[path = "../../../desktop/apps/term/src/session_fd.rs"]
mod session_fd;

use cosmix_bus::native_session::*;
use cosmix_client::session::{ExpectedScope, GrantResult};
use cosmix_client::{NodedClient, UnixConnectOutcome, VerifiedConnection};
use ed25519_dalek::SigningKey;
use session_fd::{LaunchFd, fresh_key};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::time::{Duration, Instant};
use term_native_test_broker::Broker;

fn current_mix() -> &'static std::path::Path {
    static BINARY: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    BINARY
        .get_or_init(|| {
            // Cargo builds this target before running integration tests. Never
            // search PATH, installed prefixes or guessed target directories.
            let binary = std::path::PathBuf::from(env!("CARGO_BIN_EXE_mix"));
            assert!(
                binary.is_absolute() && binary.is_file(),
                "CURRENT Mix binary unavailable: {}",
                binary.display()
            );
            let revision = std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(env!("CARGO_MANIFEST_DIR"))
                .output()
                .expect("git is required to verify the fixture's source revision");
            assert!(
                revision.status.success(),
                "cannot resolve CURRENT branch revision"
            );
            let expected = String::from_utf8(revision.stdout).unwrap();
            let output = std::process::Command::new(&binary)
                .args(["--version", "--json"])
                .env_remove(session_fd::MARKER)
                .env("MIX_STATS", "off")
                .output()
                .expect("cannot execute CURRENT Cargo-built Mix binary");
            assert!(output.status.success(), "Mix provenance probe failed");
            let provenance: serde_json::Value = serde_json::from_slice(&output.stdout)
                .expect("Mix binary must report structured build provenance");
            assert_eq!(
                provenance["git_sha_full"].as_str(),
                Some(expected.trim()),
                "STALE Mix binary: rebuild this branch; installed/stale binaries are forbidden"
            );
            assert_eq!(
                provenance["git_dirty"], false,
                "fixture requires a clean committed Mix build"
            );
            eprintln!(
                "native-session fixture binary={} commit={}",
                binary.display(),
                expected.trim()
            );
            binary
        })
        .as_path()
}

async fn connect(broker: &Broker) -> VerifiedConnection {
    match NodedClient::connect_unix("", &broker.url, &broker.options(), None)
        .await
        .unwrap()
    {
        UnixConnectOutcome::VerifiedUnix(connection) => connection,
        _ => panic!("verified UDS required"),
    }
}

struct Parent {
    key: SigningKey,
    connection: VerifiedConnection,
    record: SessionRecord,
    last_renew: Instant,
}
impl Parent {
    async fn new(broker: &Broker) -> Self {
        let key = fresh_key().unwrap();
        let connection = connect(broker).await;
        let record = connection
            .session_allocate(&key, Policy::DefaultOpen)
            .await
            .unwrap()
            .record;
        Self {
            key,
            connection,
            record,
            last_renew: Instant::now(),
        }
    }
    async fn grant(&self, key: HexBytes<32>, generation: u64) -> GrantResult {
        self.connection
            .session_grant_create(&GrantCreateArgs {
                parent: self.record.reference(),
                pane_id: DecimalU64(1),
                pane_generation: DecimalU64(generation),
                public_key: key,
                role: Role::PaneShell,
                capabilities: vec![Capability::ReadState],
            })
            .await
            .unwrap()
    }
    async fn renew(&mut self) {
        if self.last_renew.elapsed() >= Duration::from_secs(4) {
            self.record = self
                .connection
                .session_renew(self.record.reference())
                .await
                .unwrap()
                .record;
            self.last_renew = Instant::now();
        }
    }
    async fn resume(&mut self, broker: &Broker) {
        self.connection.client().close().await;
        self.connection = connect(broker).await;
        let hello = self.connection.session_hello().await.unwrap();
        let expected = ExpectedScope {
            broker_epoch: hello.broker_epoch,
            purpose: Purpose::Resume,
            unix_uid: self.record.owner_uid,
            parent_key_hash: None,
            pane_id: None,
            pane_high_water: None,
            role: Role::Term,
            public_key_hash: HexBytes(Sha256::digest(self.key.verifying_key().to_bytes()).into()),
            capabilities_hash: HexBytes(
                Sha256::digest(encode_capabilities(&self.record.capabilities).unwrap()).into(),
            ),
        };
        let challenge = self
            .connection
            .session_challenge_key(HexBytes(self.key.verifying_key().to_bytes()))
            .await
            .unwrap();
        self.record = self
            .connection
            .session_prove(&challenge.sign(&self.key, &expected).unwrap())
            .await
            .unwrap()
            .record;
        self.last_renew = Instant::now();
    }
    async fn replace(&mut self, broker: &Broker) {
        self.connection = connect(broker).await;
        self.record = self
            .connection
            .session_allocate(&self.key, Policy::DefaultOpen)
            .await
            .unwrap()
            .record;
        self.last_renew = Instant::now();
    }
    async fn revoke_and_verify(&self, broker: &Broker) {
        // Self-revoke closes this attachment in noded before its ACK is
        // guaranteed to arrive. Verify the committed state independently;
        // ignoring a transport error alone would hide a failed revocation.
        let outcome = self
            .connection
            .session_revoke(self.record.reference())
            .await;
        let observer = connect(broker).await;
        let records = observer.session_list().await.unwrap().records;
        assert!(
            records.iter().any(|record| {
                record.record_id == self.record.record_id && record.state == BindingState::Revoked
            }),
            "parent self-revoke did not commit: {outcome:?}"
        );
    }
    async fn wait(
        &mut self,
        id: HexBytes<16>,
        state: BindingState,
        generation: u64,
    ) -> SessionRecord {
        let deadline = Instant::now() + Duration::from_secs(25);
        loop {
            self.renew().await;
            let list = self.connection.session_list().await.unwrap();
            if let Some(record) = list.records.into_iter().find(|record| {
                record.record_id == id
                    && record.state == state
                    && record.binding_generation.0 >= generation
            }) {
                return record;
            }
            assert!(
                Instant::now() < deadline,
                "child did not reach {state:?}, generation {generation}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

struct Child {
    pty: teletypewriter::Pty,
    home: tempfile::TempDir,
    reaped: bool,
}
impl Child {
    fn spawn(broker: &Broker, launch: &LaunchFd) -> Self {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join("node.conf.mix");
        std::fs::write(
            &config,
            format!(
                "noded: {{ unix_socket: {} }}\n",
                serde_json::to_string(&broker.endpoint).unwrap()
            ),
        )
        .unwrap();
        // Startup rc must already see a scrubbed environment. A fresh child
        // spawned FROM the enrolled shell must see neither seed fd nor marker.
        std::fs::write(
            home.path().join(".mixrc"),
            "print(\"RC_MARKER=[\" .. env(\"COSMIX_SESSION_FD\") .. \"]\")\n",
        )
        .unwrap();
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        let mut buffer = vec![0u8; 65536];
        assert_eq!(
            unsafe {
                libc::getpwuid_r(
                    libc::geteuid(),
                    entry.as_mut_ptr(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    &mut result,
                )
            },
            0
        );
        assert!(!result.is_null());
        let entry = unsafe { entry.assume_init() };
        let account = unsafe { std::ffi::CStr::from_ptr(entry.pw_name) }
            .to_str()
            .unwrap()
            .to_owned();
        let env = vec![
            launch.marker(),
            ("HOME".into(), home.path().display().to_string()),
            ("COSMIX_SRC".into(), home.path().display().to_string()),
            ("COSMIX_NODE_CONFIG".into(), config.display().to_string()),
            ("COSMIX_BROKER_ACCOUNT".into(), account),
            ("MIX_STATS".into(), "off".into()),
            ("MIX_EDITOR".into(), "owned".into()),
            ("TERM".into(), "xterm-256color".into()),
        ];
        let pty = teletypewriter::create_pty_with_spawn_fd(
            Some(current_mix().to_str().unwrap()),
            vec![],
            &Some(home.path().display().to_string()),
            Some(env),
            100,
            30,
            1000,
            600,
            Some(launch.mapping()),
        )
        .unwrap();
        Self {
            pty,
            home,
            reaped: false,
        }
    }
    fn pid(&self) -> i32 {
        *self.pty.child.pid
    }
    fn send(&mut self, line: &str) {
        self.pty.write_all(line.as_bytes()).unwrap();
    }
    fn until(&mut self, marker: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut output = String::new();
        while !output.contains(marker) {
            let mut bytes = [0; 8192];
            match self.pty.read(&mut bytes) {
                Ok(n) if n > 0 => output.push_str(&String::from_utf8_lossy(&bytes[..n])),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                other => panic!("PTY ended: {other:?}; {output}"),
            }
            assert!(Instant::now() < deadline, "waiting for {marker}: {output}");
            std::thread::sleep(Duration::from_millis(10));
        }
        output
    }
    fn context(&mut self) -> serde_json::Value {
        self.send("mix context\nprint(\"CONTEXT_DONE\")\n");
        // The owned editor emits mode/reset escapes between the preceding
        // newline and command output. Match the output suffix, not adjacency
        // to that newline. Echoed source ends in `")`, so it cannot match.
        let output = self.until("CONTEXT_DONE\r\n");
        let start = output.find("{\r\n").expect("context JSON");
        let end = output[start..].find("\r\n}\r\n").unwrap() + start + 3;
        serde_json::from_str(&output[start..end]).unwrap()
    }
    fn exit(&mut self) {
        self.send("exit\n");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut status = 0;
            let result = unsafe { libc::waitpid(self.pid(), &mut status, libc::WNOHANG) };
            if result == self.pid() {
                self.reaped = true;
                assert!(libc::WIFEXITED(status));
                assert_eq!(libc::WEXITSTATUS(status), 0);
                return;
            }
            assert!(result >= 0 && Instant::now() < deadline, "Mix did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Child {
    fn drop(&mut self) {
        if !self.reaped {
            unsafe {
                libc::kill(self.pid(), libc::SIGKILL);
                libc::waitpid(self.pid(), std::ptr::null_mut(), 0);
            }
        }
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn mix_child_bootstrap_proves_end_to_end() {
    let broker = Broker::start();
    runtime().block_on(async {
        let mut parent = Parent::new(&broker).await;
        let key = fresh_key().unwrap();
        let public_key = HexBytes(key.verifying_key().to_bytes());
        let grant = parent.grant(public_key, 1).await;
        let launch = LaunchFd::new(&grant, &key).unwrap();
        let mut child = Child::spawn(&broker, &launch);
        drop(launch);
        drop(key); // fixture parent retains only the public key, as Term does
        let startup = child.until("RC_MARKER=[]\r\n");
        assert!(!startup.contains("native-session FAILED"), "{startup}");
        let bound = parent
            .wait(grant.record.record_id, BindingState::Attached, 1)
            .await;
        assert_eq!(bound.binding_generation, DecimalU64(1));
        let context = child.context();
        let context = serde_json::to_string(&context).unwrap();
        for forbidden in [
            "COSMIX_SESSION_FD",
            "native_session",
            "session_seed",
            "SigningKey",
            "Zeroizing",
        ] {
            assert!(
                !context.contains(forbidden),
                "private bootstrap state in context"
            );
        }
        // No memfd remains after startup, and a later exec observes no marker.
        let descriptors = std::fs::read_dir(format!("/proc/{}/fd", child.pid())).unwrap();
        assert!(!descriptors.filter_map(Result::ok).any(|entry| {
            std::fs::read_link(entry.path())
                .is_ok_and(|target| target.to_string_lossy().contains("memfd:cosmix-session"))
        }));
        let helper = child.home.path().join("descendant.mix");
        std::fs::write(
            &helper,
            "print(\"DESCENDANT=[\" .. env(\"COSMIX_SESSION_FD\") .. \"]\")\n",
        )
        .unwrap();
        child.send(&format!(
            "print(run_argv([{}, {}]).stdout)\n",
            serde_json::to_string(current_mix()).unwrap(),
            serde_json::to_string(&helper).unwrap()
        ));
        child.until("DESCENDANT=[]\r\n");
        // Longer than the 15s initial child lease; only Mix's resident renews
        // can keep it attached. The fixture renews ONLY its parent record.
        let until = Instant::now() + Duration::from_secs(17);
        while Instant::now() < until {
            parent.renew().await;
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        parent
            .wait(bound.record_id, BindingState::Attached, 1)
            .await;
        child.exit();
        let latest = parent
            .connection
            .session_grant_fetch(public_key)
            .await
            .unwrap()
            .record;
        // Real waitpid completion drives the fixture's Term-owned revoke.
        parent
            .connection
            .session_revoke(latest.reference())
            .await
            .unwrap();
        parent.wait(bound.record_id, BindingState::Revoked, 1).await;
        parent.revoke_and_verify(&broker).await;
    });
}

#[test]
fn same_mix_child_resumes_and_reenrols_after_broker_bounce() {
    let mut broker = Broker::start();
    runtime().block_on(async {
        let mut parent = Parent::new(&broker).await;
        let key = fresh_key().unwrap();
        let public_key = HexBytes(key.verifying_key().to_bytes());
        let initial = parent.grant(public_key, 2).await;
        let launch = LaunchFd::new(&initial, &key).unwrap();
        let mut child = Child::spawn(&broker, &launch);
        drop(launch);
        drop(key);
        child.until("RC_MARKER=[]\r\n");
        let first = parent
            .wait(initial.record.record_id, BindingState::Attached, 1)
            .await;
        let pid = child.pid();
        parent.resume(&broker).await;
        let resumed = parent
            .wait(first.record_id, BindingState::Attached, 2)
            .await;
        assert_eq!(resumed.pane_generation, Some(DecimalU64(2)));
        broker.bounce();
        parent.replace(&broker).await;
        // Let the child connect first and retain key interest while no grant
        // exists. A later Term re-grant must wake it without a polling timer.
        let wait = Instant::now() + Duration::from_secs(12);
        while Instant::now() < wait {
            parent.renew().await;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let replacement = parent.grant(public_key, 1).await;
        let rebound = parent
            .wait(replacement.record.record_id, BindingState::Attached, 1)
            .await;
        assert_eq!(child.pid(), pid);
        assert_ne!(rebound.broker_epoch, first.broker_epoch);
        assert_ne!(rebound.record_id, first.record_id);
        // BROKER-020 resets binding generation to 1 for the NEW record; only
        // same-record resumption above increments it. Parent key continuity
        // permits resetting the old pane high-water of 2 to the new parent's 1.
        assert_eq!(rebound.binding_generation, DecimalU64(1));
        assert_eq!(rebound.pane_generation, Some(DecimalU64(1)));
        child.send("print(\"SAME_CHILD_ALIVE\")\n");
        child.until("SAME_CHILD_ALIVE\r\n");
        child.exit();
        parent
            .connection
            .session_revoke(rebound.reference())
            .await
            .unwrap();
        parent
            .wait(rebound.record_id, BindingState::Revoked, 1)
            .await;
        parent.revoke_and_verify(&broker).await;
    });
}

#[test]
fn valid_handoff_with_broker_down_does_not_delay_first_source() {
    let mut broker = Broker::start();
    runtime().block_on(async {
        let parent = Parent::new(&broker).await;
        let key = fresh_key().unwrap();
        let grant = parent
            .grant(HexBytes(key.verifying_key().to_bytes()), 1)
            .await;
        let launch = LaunchFd::new(&grant, &key).unwrap();
        broker.stop();
        let start = Instant::now();
        let mut child = Child::spawn(&broker, &launch);
        drop(launch);
        drop(key);
        let mut output = child.until("RC_MARKER=[]\r\n");
        assert!(start.elapsed() < Duration::from_secs(1));
        if !output.contains("mix native-session FAILED at connect:") {
            output.push_str(&child.until("mix native-session FAILED at connect:"));
        }
        child.send("print(\"UNBOUND_WORKS\")\n");
        output.push_str(&child.until("UNBOUND_WORKS\r\n"));
        assert_eq!(output.matches("mix native-session FAILED").count(), 1);
        child.exit();
    });
}

#[test]
fn substituted_parent_scope_is_rejected_without_failing_shell() {
    let broker = Broker::start();
    runtime().block_on(async {
        let mut parent = Parent::new(&broker).await;
        let key = fresh_key().unwrap();
        let mut grant = parent
            .grant(HexBytes(key.verifying_key().to_bytes()), 1)
            .await;
        grant.grant.parent_key_hash = HexBytes([0; 32]);
        let launch = LaunchFd::new(&grant, &key).unwrap();
        let mut child = Child::spawn(&broker, &launch);
        drop(launch);
        drop(key);
        let mut output = child.until("RC_MARKER=[]\r\n");
        if !output.contains("mix native-session FAILED at scope:") {
            output.push_str(&child.until("mix native-session FAILED at scope:"));
        }
        child.send("print(\"REFUSED_WORKS\")\n");
        output.push_str(&child.until("REFUSED_WORKS\r\n"));
        assert_eq!(output.matches("mix native-session FAILED").count(), 1);
        parent
            .wait(grant.record.record_id, BindingState::Pending, 0)
            .await;
        child.exit();
        parent.revoke_and_verify(&broker).await;
    });
}

#[test]
fn bootstrap_has_no_evaluator_or_builtin_route() {
    let owner = include_str!("../src/native_session.rs");
    for forbidden in [
        "cosmix_mix::",
        "Evaluator",
        "pub struct Bootstrap",
        "pub fn seed",
        "impl Clone for Bootstrap",
        "Serialize for Bootstrap",
    ] {
        assert!(!owner.contains(forbidden), "owner exposes {forbidden}");
    }
    let source = include_str!("../src/main.rs");
    let main = source.split("fn main() {").nth(1).unwrap();
    assert!(main.find("native_session::start()").unwrap() < main.find("spawn(real_main)").unwrap());
    let output = std::process::Command::new(current_mix())
        .args(["builtins", "--json"])
        .env_remove(session_fd::MARKER)
        .env("MIX_STATS", "off")
        .output()
        .unwrap();
    assert!(output.status.success());
    let builtins = String::from_utf8(output.stdout).unwrap();
    for forbidden in ["COSMIX_SESSION_FD", "native_session", "session_seed"] {
        assert!(!builtins.contains(forbidden));
    }
}
