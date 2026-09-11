//! S3: real noded + Term's real LaunchFd/PTY mapping + the built Mix binary.
//! The fixture owns the Term-side record/renew/re-grant/revoke duties. Term's
//! GUI mutation and exit-notifier ordering remain covered in its own workspace.
//! A process-wide fixture lock excludes sibling forks across openpty/dup/spawn.
#![cfg(target_os = "linux")]

use cosmix_lib_bus::native_session::*;
use cosmix_lib_client::session::{ExpectedScope, GrantResult};
use cosmix_lib_client::{NodedClient, UnixConnectOutcome, VerifiedConnection};
use ed25519_dalek::SigningKey;
use session_fd::{LaunchFd, fresh_key};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::time::{Duration, Instant};
use term_native_test_broker::Broker;
use term_native_test_broker::session_fd;

static FIXTURE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn fixture_guard() -> std::sync::MutexGuard<'static, ()> {
    FIXTURE_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

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
            // Build-script git_dirty can be stale after dependency-only edits.
            let status = Command::new("git")
                .args(["status", "--porcelain", "--untracked-files=normal"])
                .current_dir(env!("CARGO_MANIFEST_DIR"))
                .output()
                .expect("git status is required for fixture provenance");
            assert!(
                status.status.success() && status.stdout.is_empty(),
                "fixture requires a clean CURRENT checkout, including dependency edits"
            );
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
    pty: File,
    process: std::process::Child,
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
            (
                "COSMIX_BIN".into(),
                current_mix().parent().unwrap().display().to_string(),
            ),
            ("COSMIX_NODE_CONFIG".into(), config.display().to_string()),
            ("COSMIX_BROKER_ACCOUNT".into(), account),
            ("MIX_STATS".into(), "off".into()),
            ("MIX_EDITOR".into(), "owned".into()),
            ("TERM".into(), "xterm-256color".into()),
        ];
        // Same libc PTY pattern as job_control_pty, with the real LaunchFd's
        // reserved mapping duplicated only in this child's pre_exec hook.
        let (mut master_fd, mut slave_fd) = (-1, -1);
        let size = libc::winsize {
            ws_row: 30,
            ws_col: 100,
            ws_xpixel: 1000,
            ws_ypixel: 600,
        };
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master_fd,
                    &mut slave_fd,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    &size,
                )
            },
            0
        );
        for fd in [&mut master_fd, &mut slave_fd] {
            let retained = unsafe { libc::fcntl(*fd, libc::F_DUPFD_CLOEXEC, 3) };
            assert!(retained >= 0);
            unsafe {
                libc::close(*fd);
            }
            *fd = retained;
        }
        let pty = unsafe { File::from_raw_fd(master_fd) };
        let slave = unsafe { File::from_raw_fd(slave_fd) };
        let flags = unsafe { libc::fcntl(master_fd, libc::F_GETFL) };
        assert!(flags >= 0);
        assert_eq!(
            unsafe { libc::fcntl(master_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0
        );
        let mut command = Command::new(current_mix());
        command
            .current_dir(home.path())
            .envs(env)
            .stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave);
        let (source, target) = launch.mapping();
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::dup2(source, target) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let process = command.spawn().unwrap();
        Self {
            pty,
            process,
            home,
            reaped: false,
        }
    }
    fn pid(&self) -> i32 {
        self.process.id() as i32
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
    let _fixture = fixture_guard();
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
        let still_bound = parent
            .wait(bound.record_id, BindingState::Attached, 1)
            .await;
        assert_eq!(
            still_bound.reference(),
            bound.reference(),
            "renew must preserve the ORIGINAL attachment; resumption cannot substitute"
        );
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
    let _fixture = fixture_guard();
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
    let _fixture = fixture_guard();
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
    let _fixture = fixture_guard();
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
fn enrolled_exec_restart_revokes_and_replacement_stays_unbound() {
    let _fixture = fixture_guard();
    let broker = Broker::start();
    runtime().block_on(async {
        let mut parent = Parent::new(&broker).await;
        let key = fresh_key().unwrap();
        let grant = parent
            .grant(HexBytes(key.verifying_key().to_bytes()), 1)
            .await;
        let launch = LaunchFd::new(&grant, &key).unwrap();
        let mut child = Child::spawn(&broker, &launch);
        drop(launch);
        drop(key);
        child.until("RC_MARKER=[]\r\n");
        let bound = parent
            .wait(grant.record.record_id, BindingState::Attached, 1)
            .await;
        let pid = child.pid();
        // Exercise the real repl.rs exec_restart path, with the existing
        // self-update resume flag. Empty contents suppress a resumed command.
        std::fs::write(child.home.path().join(".claude-resume"), "").unwrap();
        child.send("/usr/bin/true\n");
        let output = child.until("RC_MARKER=[]\r\n");
        assert_eq!(
            output
                .matches("exec restart leaves this pane unbound")
                .count(),
            1,
            "{output}"
        );
        assert!(output.contains("record revoked"), "{output}");
        assert_eq!(child.pid(), pid);
        parent.wait(bound.record_id, BindingState::Revoked, 1).await;
        child.send("print(\"REPLACEMENT_WORKS\")\n");
        let output = child.until("REPLACEMENT_WORKS\r\n");
        assert!(
            !output.contains("native-session"),
            "replacement must stay silently unbound"
        );
        assert!(
            !serde_json::to_string(&child.context())
                .unwrap()
                .contains("COSMIX_SESSION_FD")
        );
        child.exit();
        parent.revoke_and_verify(&broker).await;
    });
}

#[test]
fn bootstrap_source_boundary_and_builtin_inventory_exclude_seed_state() {
    let _fixture = fixture_guard();
    let owner = include_str!("../src/native_session.rs");
    // Load-bearing boundary: the secret owner never imports the evaluator lib.
    assert!(!owner.contains("cosmix_mix::"));
    for forbidden in [
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
