//! S3: real noded + Term's real LaunchFd/PTY mapping + the built Mix binary.
//! The fixture owns the Term-side record/renew/re-grant/revoke duties. Term's
//! GUI mutation and exit-notifier ordering remain covered in its own workspace.
//! A process-wide fixture lock excludes sibling forks across openpty/dup/spawn.
//! It spans each whole fixture: same-process runs include lock wait in latency.
//! Nextest uses separate test processes (no shared mutex); its timeout budget
//! still needs to allow setup plus the real lease/retry fixture durations.
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
                .args(["status", "--porcelain", "--untracked-files=no"])
                .current_dir(env!("CARGO_MANIFEST_DIR"))
                .output()
                .expect("git status is required for fixture provenance");
            assert!(
                status.status.success() && status.stdout.is_empty(),
                "fixture provenance requires no tracked checkout changes (including dependencies); untracked files are ignored. git status: {}{}",
                String::from_utf8_lossy(&status.stdout), String::from_utf8_lossy(&status.stderr)
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
    connection: std::sync::Arc<VerifiedConnection>,
    record: SessionRecord,
    last_renew: Instant,
}
impl Parent {
    async fn new(broker: &Broker) -> Self {
        Self::with_policy(broker, Policy::DefaultOpen).await
    }
    async fn with_policy(broker: &Broker, policy: Policy) -> Self {
        let key = fresh_key().unwrap();
        let connection = connect(broker).await;
        let record = connection
            .session_allocate(&key, policy)
            .await
            .unwrap()
            .record;
        Self {
            key,
            connection: connection.into(),
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
        self.connection = connect(broker).await.into();
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
        self.connection = connect(broker).await.into();
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
        Self::spawn_editor(broker, launch, "owned")
    }
    fn spawn_editor(broker: &Broker, launch: &LaunchFd, editor: &str) -> Self {
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
            ("MIX_EDITOR".into(), editor.into()),
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
    for editor in ["owned", "legacy"] {
        same_mix_child_scenarios(editor);
    }
}

fn same_mix_child_scenarios(editor: &str) {
    let _fixture = fixture_guard();
    let mut broker = Broker::start();
    runtime().block_on(async {
        let mut parent = Parent::new(&broker).await;
        let key = fresh_key().unwrap();
        let public_key = HexBytes(key.verifying_key().to_bytes());
        let initial = parent.grant(public_key, 2).await;
        let launch = LaunchFd::new(&initial, &key).unwrap();
        let mut child = Child::spawn_editor(&broker, &launch, editor);
        drop(launch);
        drop(key);
        child.until("RC_MARKER=[]\r\n");
        let first = parent
            .wait(initial.record.record_id, BindingState::Attached, 1)
            .await;
        let before = phase(&mut parent, &first, "prompt-ready").await;
        let pid = child.pid();
        parent.resume(&broker).await;
        let resumed = parent
            .wait(first.record_id, BindingState::Attached, 2)
            .await;
        assert_eq!(resumed.pane_generation, Some(DecimalU64(2)));
        let after = phase(&mut parent, &resumed, "prompt-ready").await;
        assert!(
            counter(&after["status"]["snapshot"]["sequence"])
                > counter(&before["status"]["snapshot"]["sequence"])
        );
        assert_eq!(
            after["status"]["snapshot"]["prompt_generation"],
            before["status"]["snapshot"]["prompt_generation"]
        );
        let stale = parent
            .connection
            .client()
            .call(&resumed.name, "shell.status", status_request(&first))
            .await
            .unwrap_err();
        assert!(stale.to_string().contains("STALE_GENERATION"));
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
        let recovered = phase(&mut parent, &rebound, "prompt-ready").await;
        assert!(
            counter(&recovered["status"]["snapshot"]["sequence"])
                > counter(&after["status"]["snapshot"]["sequence"])
        );
        assert_eq!(
            recovered["status"]["snapshot"]["prompt_generation"],
            after["status"]["snapshot"]["prompt_generation"]
        );
        let stale = parent
            .connection
            .client()
            .call(&rebound.name, "shell.status", status_request(&resumed))
            .await
            .unwrap_err();
        assert!(stale.to_string().contains("STALE_GENERATION"));
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
        // Account NSS lookup and config-file reads are resident-only again;
        // neither name-service latency nor broker retry delays first source.
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
        assert!(output.contains("record observed revoked"), "{output}");
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

fn status_request(record: &SessionRecord) -> serde_json::Value {
    serde_json::json!({"version":1,"target":{
        "broker_epoch":record.broker_epoch,"record":record.reference(),
        "instance_id":record.instance_id,"pane_id":record.pane_id,
        "pane_generation":record.pane_generation
    }})
}

fn counter(value: &serde_json::Value) -> u64 {
    value
        .as_str()
        .expect("decimal-string counter")
        .parse()
        .unwrap()
}

async fn status(parent: &mut Parent, record: &SessionRecord) -> serde_json::Value {
    parent.renew().await;
    let start = Instant::now();
    let value = tokio::time::timeout(
        Duration::from_secs(3),
        parent
            .connection
            .client()
            .call(&record.name, "shell.status", status_request(record)),
    )
    .await
    .expect("status blocked behind shell activity")
    .unwrap();
    eprintln!(
        "status response {:?}, phase={}",
        start.elapsed(),
        value["status"]["snapshot"]["phase"]
    );
    assert_eq!(value["version"], 1);
    assert_eq!(
        value["status"]["snapshot"]["source"],
        status_request(record)["target"]
    );
    assert!(
        value["freshness"]
            .as_str()
            .unwrap()
            .contains("never an execution permit")
    );
    value
}

async fn phase(parent: &mut Parent, record: &SessionRecord, expected: &str) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let value = status(parent, record).await;
        if value["status"]["snapshot"]["phase"] == expected {
            return value;
        }
        assert!(Instant::now() < deadline, "expected {expected}: {value}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[test]
fn status_pump_answers_idle_pure_loop_and_blocking_builtin() {
    for editor in ["owned", "legacy"] {
        status_pump_scenarios(editor);
    }
}

fn status_pump_scenarios(editor: &str) {
    let _fixture = fixture_guard();
    let broker = Broker::start();
    runtime().block_on(async {
        let mut parent = Parent::new(&broker).await;
        let key = fresh_key().unwrap();
        let grant = parent
            .grant(HexBytes(key.verifying_key().to_bytes()), 1)
            .await;
        let launch = LaunchFd::new(&grant, &key).unwrap();
        let mut child = Child::spawn_editor(&broker, &launch, editor);
        drop(launch);
        drop(key);
        child.until("RC_MARKER=[]\r\n");
        let bound = parent
            .wait(grant.record.record_id, BindingState::Attached, 1)
            .await;
        let idle = phase(&mut parent, &bound, "prompt-ready").await;
        let owner = connect(&broker).await;
        let owner_view = tokio::time::timeout(
            Duration::from_secs(3),
            owner
                .client()
                .call(&bound.name, "shell.status", status_request(&bound)),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            owner_view["status"]["snapshot"]["source"],
            status_request(&bound)["target"]
        );
        owner.client().close().await;
        assert_eq!(
            idle["status"]["snapshot"]["command_id"],
            serde_json::Value::Null
        );
        let initial_sequence = counter(&idle["status"]["snapshot"]["sequence"]);
        let initial_prompt = counter(&idle["status"]["snapshot"]["prompt_generation"]);
        assert_eq!(
            idle["status"]["snapshot"]["cwd"],
            child.home.path().to_str().unwrap()
        );
        // No keystroke is needed for either request, and idle does not produce
        // synthetic transitions. Freshness ages still advance between samples.
        tokio::time::sleep(Duration::from_millis(30)).await;
        let still_idle = status(&mut parent, &bound).await;
        assert_eq!(
            counter(&still_idle["status"]["snapshot"]["sequence"]),
            initial_sequence
        );
        assert!(
            counter(&still_idle["status"]["transition_age_ms"])
                > counter(&idle["status"]["transition_age_ms"])
        );
        for feature in [
            "jobs",
            "job_signal",
            "foreground",
            "input",
            "isolated_task",
            "events",
        ] {
            assert_eq!(idle["capabilities"][feature], "UNSUPPORTED");
        }
        // Stage D turned the two evaluation families on, and only where the
        // terminal can be released without a keypress. The report is derived
        // from what this build can do, so the two editors must disagree here.
        let (submit, inspect) = if editor == "owned" {
            ("idle-prompt-admission", "result-and-cancel")
        } else {
            ("UNSUPPORTED", "UNSUPPORTED")
        };
        assert_eq!(idle["capabilities"]["evaluation_submit"], submit);
        assert_eq!(idle["capabilities"]["evaluation_inspect"], inspect);

        child.send("while true; 1 + 1; done\n");
        let evaluating = phase(&mut parent, &bound, "evaluating").await;
        let command = counter(&evaluating["status"]["snapshot"]["command_id"]);
        let second = status(&mut parent, &bound).await;
        assert_eq!(
            second["status"]["snapshot"]["command_id"],
            evaluating["status"]["snapshot"]["command_id"]
        );
        assert_eq!(second["status"]["snapshot"]["phase"], "evaluating");
        unsafe {
            assert_eq!(libc::kill(child.pid(), libc::SIGINT), 0);
        }
        let next = phase(&mut parent, &bound, "prompt-ready").await;
        assert!(counter(&next["status"]["snapshot"]["prompt_generation"]) > initial_prompt);

        child.send("run_stream([\"/bin/sleep\", \"30\"])\n");
        let foreground = phase(&mut parent, &bound, "foreground-child").await;
        assert!(counter(&foreground["status"]["snapshot"]["command_id"]) > command);
        assert_eq!(
            status(&mut parent, &bound).await["status"]["snapshot"]["phase"],
            "foreground-child"
        );
        child.send("\x03");
        phase(&mut parent, &bound, "prompt-ready").await;

        // Both directory mutation paths emit transitions even before an
        // evaluation finishes; neither requires prompt-time cwd polling.
        let directory = child.home.path().join("directory");
        std::fs::create_dir(&directory).unwrap();
        child.send("cd directory\n");
        let cd = phase(&mut parent, &bound, "prompt-ready").await;
        // The next query below waits on cwd too, avoiding an old prompt race.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut cd = cd;
        while cd["status"]["snapshot"]["cwd"] != directory.to_str().unwrap() {
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(25)).await;
            cd = status(&mut parent, &bound).await;
        }
        child.send("chdir(\"..\"); print(\"CWD_CHANGED\"); while true; 1 + 1; done\n");
        child.until("CWD_CHANGED\r\n");
        let changed = phase(&mut parent, &bound, "evaluating").await;
        assert_eq!(
            changed["status"]["snapshot"]["cwd"],
            child.home.path().to_str().unwrap()
        );
        unsafe {
            assert_eq!(libc::kill(child.pid(), libc::SIGINT), 0);
        }
        phase(&mut parent, &bound, "prompt-ready").await;
        child.exit();
        parent
            .connection
            .session_revoke(bound.reference())
            .await
            .unwrap();
        parent.revoke_and_verify(&broker).await;
    });
}

#[test]
fn status_restricted_identity_rejection_and_unsupported_verbs() {
    for editor in ["owned", "legacy"] {
        status_restricted_scenarios(editor);
    }
}

fn status_restricted_scenarios(editor: &str) {
    let _fixture = fixture_guard();
    let broker = Broker::start();
    runtime().block_on(async {
        let mut parent = Parent::with_policy(&broker, Policy::Restricted).await;
        let key = fresh_key().unwrap();
        let grant = parent
            .grant(HexBytes(key.verifying_key().to_bytes()), 1)
            .await;
        let launch = LaunchFd::new(&grant, &key).unwrap();
        let mut child = Child::spawn_editor(&broker, &launch, editor);
        drop(launch);
        drop(key);
        child.until("RC_MARKER=[]\r\n");
        let bound = parent
            .wait(grant.record.record_id, BindingState::Attached, 1)
            .await;
        phase(&mut parent, &bound, "prompt-ready").await;
        let ambient = connect(&broker).await;
        let foreign = Parent::new(&broker).await;
        for connection in [&ambient, foreign.connection.as_ref()] {
            let result = tokio::time::timeout(
                Duration::from_millis(400),
                connection
                    .client()
                    .call(&bound.name, "shell.status", status_request(&bound)),
            )
            .await;
            let error = result
                .expect("denial must reply, not time out")
                .unwrap_err();
            assert_eq!(error.to_string(), r#"{"error_code":"REFUSED"}"#);
        }
        let tcp = NodedClient::connect_anonymous(&broker.url).await.unwrap();
        let result = tokio::time::timeout(
            Duration::from_millis(400),
            tcp.call(&bound.name, "shell.status", status_request(&bound)),
        )
        .await;
        let error = result
            .expect("unverified denial must reply, not time out")
            .unwrap_err();
        assert_eq!(error.to_string(), r#"{"error_code":"REFUSED"}"#);
        tcp.close().await;
        for verb in [
            "shell.jobs",
            "shell.evaluate",
            "shell.evaluation.inspect",
            "shell.input",
            "shell.foreground",
        ] {
            let error = tokio::time::timeout(
                Duration::from_secs(3),
                parent
                    .connection
                    .client()
                    .call(&bound.name, verb, status_request(&bound)),
            )
            .await
            .unwrap()
            .unwrap_err();
            assert!(error.to_string().contains("UNSUPPORTED"), "{error}");
            assert!(!error.to_string().contains("BUSY"));
        }
        let mut stale = status_request(&bound);
        stale["target"]["pane_generation"] = serde_json::json!("999");
        let error = parent
            .connection
            .client()
            .call(&bound.name, "shell.status", stale)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("STALE_GENERATION"));
        let mut malformed = status_request(&bound);
        malformed["extra"] = serde_json::json!(true);
        let error = parent
            .connection
            .client()
            .call(&bound.name, "shell.status", malformed)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("INVALID_REQUEST"));
        ambient.client().close().await;
        foreign.revoke_and_verify(&broker).await;
        child.exit();
        parent
            .connection
            .session_revoke(bound.reference())
            .await
            .unwrap();
        parent.revoke_and_verify(&broker).await;
    });
}

#[test]
fn status_verbs_are_absent_from_legacy_surfaces() {
    for source in [
        include_str!("../src/bus.rs"),
        include_str!("../src/serve_runtime.rs"),
        include_str!("../src/meta.rs"),
        include_str!("../../cosmix-lib-mix/src/builtins.rs"),
    ] {
        for verb in [
            "shell.status",
            "shell.jobs",
            "shell.evaluate",
            "shell.input",
        ] {
            assert!(!source.contains(verb), "legacy surface contains {verb}");
        }
    }
}

#[test]
fn status_flood_preserves_lease_and_restart_ack() {
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
        phase(&mut parent, &bound, "prompt-ready").await;
        let mut flood = tokio::task::JoinSet::new();
        let refused = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        // The flood is other same-UID processes, not the owning Term: this
        // child's policy admits ambient callers, so they compete for the very
        // dispatch slots the Term needs.
        let mut ambient = Vec::new();
        for _ in 0..8 {
            ambient.push(std::sync::Arc::new(connect(&broker).await));
        }
        for index in 0..64 {
            let connection = ambient[index % ambient.len()].clone();
            let target = bound.clone();
            let refused = refused.clone();
            flood.spawn(async move {
                loop {
                    let result = tokio::time::timeout(
                        Duration::from_secs(3),
                        connection.client().call(
                            &target.name,
                            "shell.status",
                            status_request(&target),
                        ),
                    )
                    .await
                    .expect("flood request must be answered or explicitly refused");
                    match result {
                        Ok(value) => assert_eq!(
                            value["status"]["snapshot"]["source"],
                            status_request(&target)["target"]
                        ),
                        Err(error) => {
                            assert_eq!(error.to_string(), r#"{"error_code":"REFUSED"}"#);
                            refused.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                    tokio::task::yield_now().await;
                }
            });
        }
        // Longer than the initial 15s child lease: only the resident's renew
        // arm can keep this exact attachment alive under a continuously full load.
        let deadline = Instant::now() + Duration::from_secs(17);
        let mut answered = 0;
        while Instant::now() < deadline {
            parent.renew().await;
            assert!(flood.try_join_next().is_none(), "flood worker failed");
            let current = parent
                .connection
                .session_self(bound.record_id)
                .await
                .unwrap()
                .record;
            assert_eq!(current.state, BindingState::Attached);
            assert_eq!(
                current.reference(),
                bound.reference(),
                "overflow must not reconnect"
            );
            // One dispatch slot is the owning Term's, so a saturating flood by
            // other same-UID callers cannot starve it into uniform refusals.
            let value = tokio::time::timeout(
                Duration::from_secs(3),
                parent.connection.client().call(
                    &bound.name,
                    "shell.status",
                    status_request(&bound),
                ),
            )
            .await
            .expect("the owning Term must be answered during a flood")
            .expect("the owning Term must not be refused during a flood");
            assert_eq!(
                value["status"]["snapshot"]["source"],
                status_request(&bound)["target"]
            );
            answered += 1;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(answered > 0);
        assert!(refused.load(std::sync::atomic::Ordering::Relaxed) > 0);
        std::fs::write(child.home.path().join(".claude-resume"), "").unwrap();
        let started = Instant::now();
        child.send("/usr/bin/true\n");
        loop {
            let current = parent
                .connection
                .session_self(bound.record_id)
                .await
                .unwrap()
                .record;
            if current.state == BindingState::Revoked {
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "restart ack starved by flood"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        flood.abort_all();
        while flood.join_next().await.is_some() {}
        for connection in &ambient {
            connection.client().close().await;
        }
        let output = child.until("RC_MARKER=[]\r\n");
        assert!(output.contains("record observed revoked"), "{output}");
        assert!(started.elapsed() < Duration::from_secs(5));
        child.exit();
        parent.revoke_and_verify(&broker).await;
    });
}

// ---------------------------------------------------------------------------
// P0-J stage D: idle-prompt execution admission and per-evaluation cancellation
//
// These drive the real thing end to end — real broker, real grant, real Mix
// child on a real PTY with the owned editor — because every interesting claim
// stage D makes is about what happens on the glass and in the shell's own
// state, neither of which a unit test can observe.

fn execute_request(
    record: &SessionRecord,
    request_id: u64,
    prompt_generation: u64,
    source: &str,
) -> serde_json::Value {
    serde_json::json!({
        "version": 1,
        "target": status_request(record)["target"],
        "request_id": request_id.to_string(),
        "prompt_generation": prompt_generation.to_string(),
        "source": source,
    })
}

fn operation_request(record: &SessionRecord, operation_id: u64) -> serde_json::Value {
    serde_json::json!({
        "version": 1,
        "target": status_request(record)["target"],
        "operation_id": operation_id.to_string(),
    })
}

/// Every execute-family call goes through here so a refusal is a value the test
/// can assert on rather than an unwrap that only says "it failed".
async fn execute_call(
    parent: &mut Parent,
    record: &SessionRecord,
    verb: &str,
    body: serde_json::Value,
) -> Result<serde_json::Value, String> {
    parent.renew().await;
    tokio::time::timeout(
        Duration::from_secs(5),
        parent.connection.client().call(&record.name, verb, body),
    )
    .await
    .expect("an execute-family request must always answer")
    .map_err(|error| error.to_string())
}

async fn prompt_generation(parent: &mut Parent, record: &SessionRecord) -> u64 {
    counter(&phase(parent, record, "prompt-ready").await["status"]["snapshot"]["prompt_generation"])
}

/// Wait for the admitted evaluation to publish its outcome. The shell answers
/// `running` until the evaluator owner records the result; that transition is
/// the only thing being waited for here.
async fn result_of(
    parent: &mut Parent,
    record: &SessionRecord,
    operation: u64,
) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let value = execute_call(
            parent,
            record,
            "shell.execute.result",
            operation_request(record, operation),
        )
        .await
        .expect("a known operation always has a state");
        if value["state"] == "finished" {
            return value;
        }
        assert_eq!(value["state"], "running", "{value}");
        assert!(Instant::now() < deadline, "result never finished: {value}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

struct Fixture {
    broker: Broker,
    parent: Parent,
    child: Child,
    bound: SessionRecord,
}

async fn stage_d_fixture(editor: &str) -> Fixture {
    let broker = Broker::start();
    let mut parent = Parent::new(&broker).await;
    let key = fresh_key().unwrap();
    let grant = parent
        .grant(HexBytes(key.verifying_key().to_bytes()), 1)
        .await;
    let launch = LaunchFd::new(&grant, &key).unwrap();
    let mut child = Child::spawn_editor(&broker, &launch, editor);
    drop(launch);
    drop(key);
    child.until("RC_MARKER=[]\r\n");
    let bound = parent
        .wait(grant.record.record_id, BindingState::Attached, 1)
        .await;
    phase(&mut parent, &bound, "prompt-ready").await;
    Fixture {
        broker,
        parent,
        child,
        bound,
    }
}

async fn teardown(mut fixture: Fixture) {
    fixture.child.exit();
    fixture
        .parent
        .connection
        .session_revoke(fixture.bound.reference())
        .await
        .unwrap();
    fixture.parent.revoke_and_verify(&fixture.broker).await;
}

/// The seven-step happy path, plus the two properties that make an admitted
/// execution accountable: the pane SAYS who ran what before it runs, and a
/// retry of an accepted submission answers with the same operation instead of
/// executing twice.
#[test]
fn stage_d_admits_at_an_idle_prompt_echoes_and_reports_a_structured_result() {
    let _fixture = fixture_guard();
    runtime().block_on(async {
        let mut f = stage_d_fixture("owned").await;
        let before = prompt_generation(&mut f.parent, &f.bound).await;
        let submission = execute_request(&f.bound, 1, before, "print(\"ADMITTED_OK\")");
        let accepted = execute_call(
            &mut f.parent,
            &f.bound,
            "shell.execute",
            submission.clone(),
        )
        .await
        .expect("an idle primary prompt admits");
        assert_eq!(accepted["status"], "accepted");
        assert_eq!(accepted["state"], "running");
        let operation = counter(&accepted["operation_id"]);
        assert!(operation > 0);

        // The visible echo is the point of step 6: a human watching this pane
        // must be able to see that something other than them ran a command, who
        // it was, and which command id to ask about — BEFORE its output.
        let output = f.child.until("ADMITTED_OK\r\n");
        let marker = format!("mix: execute #{operation} admitted for");
        let echoed = output
            .find(&marker)
            .unwrap_or_else(|| panic!("no admission echo in the pane: {output}"));
        let printed = output.find("ADMITTED_OK\r\n").unwrap();
        assert!(
            echoed < printed,
            "the echo must precede the execution it announces: {output}"
        );
        let announcement = &output[echoed..printed];
        assert!(
            announcement.contains("print(\"ADMITTED_OK\")"),
            "the echo must name the submitted source: {output}"
        );
        // The announcement names a real caller, not a type name and not an
        // empty slot: a human reading the pane has to be able to tell two
        // agents apart.
        assert!(
            announcement.contains("Term ") && !announcement.contains("HexBytes"),
            "the echo must name the principal: {announcement}"
        );

        let result = result_of(&mut f.parent, &f.bound, operation).await;
        assert_eq!(result["result"]["outcome"], "completed");
        assert_eq!(result["result"]["cancellation"]["requested"], false);
        assert_eq!(result["result"]["cancellation"]["delivered"], "none");
        // print() returns nil; the value is still typed and bounded, and its
        // truncation flag is a property of the value, not of the outcome.
        assert_eq!(result["result"]["value"]["type"], "nil");
        assert_eq!(result["result"]["value"]["truncated"], false);

        // Step 7: the shell reclaimed the terminal and built a NEW prompt. The
        // generation the admission consumed can never be admitted again.
        let after = prompt_generation(&mut f.parent, &f.bound).await;
        assert!(after > before, "{after} must be past the consumed {before}");
        let stale = execute_call(
            &mut f.parent,
            &f.bound,
            "shell.execute",
            execute_request(&f.bound, 2, before, "print(\"NEVER\")"),
        )
        .await
        .unwrap_err();
        assert!(stale.contains("STALE_GENERATION"), "{stale}");

        // BROKER-018: the identical submission replays its recorded outcome.
        let replay = execute_call(&mut f.parent, &f.bound, "shell.execute", submission)
            .await
            .expect("an accepted request id answers from the record");
        assert_eq!(counter(&replay["operation_id"]), operation);
        assert_eq!(replay["state"], "finished");
        // A different body under the same request id is a caller bug, not a
        // second execution.
        let conflict = execute_call(
            &mut f.parent,
            &f.bound,
            "shell.execute",
            execute_request(&f.bound, 1, after, "print(\"DIFFERENT\")"),
        )
        .await
        .unwrap_err();
        assert!(conflict.contains("CONFLICT"), "{conflict}");
        // Exactly one execution reached the pane.
        f.child
            .send("print(\"SWEEP\")\nprint(\"SWEEP_DONE\")\n");
        let sweep = f.child.until("SWEEP_DONE\r\n");
        assert_eq!(
            sweep.matches("ADMITTED_OK").count(),
            0,
            "a replayed retry must not execute again: {sweep}"
        );
        teardown(f).await;
    });
}

/// Section 3's refusal matrix, on the live shell. The load-bearing assertion is
/// not that BUSY comes back — it is that the human's work is untouched when it
/// does.
#[test]
fn stage_d_refuses_every_ineligible_prompt_state_without_discarding_anything() {
    let _fixture = fixture_guard();
    runtime().block_on(async {
        let mut f = stage_d_fixture("owned").await;
        let generation = prompt_generation(&mut f.parent, &f.bound).await;

        // A half-typed human line. No newline: the draft is sitting in the
        // editor, rendered to the pane.
        f.child.send("print(\"DRAFT_");
        f.child.until("DRAFT_");
        let busy = execute_call(
            &mut f.parent,
            &f.bound,
            "shell.execute",
            execute_request(&f.bound, 1, generation, "print(\"STOLEN\")"),
        )
        .await
        .unwrap_err();
        assert!(busy.contains("BUSY"), "a draft must refuse: {busy}");
        // The draft is intact: finishing it produces exactly the line the human
        // was typing, so nothing was discarded and nothing was inserted.
        f.child.send("OK\")\n");
        let typed = f.child.until("DRAFT_OK\r\n");
        assert!(
            !typed.contains("STOLEN"),
            "a refused submission must not have executed: {typed}"
        );

        // A continuation prompt. Admission is empty-PRIMARY-prompt only.
        f.child.send("if true then\n");
        tokio::time::sleep(Duration::from_millis(300)).await;
        let continuation = prompt_generation_now(&mut f.parent, &f.bound).await;
        let busy = execute_call(
            &mut f.parent,
            &f.bound,
            "shell.execute",
            execute_request(&f.bound, 2, continuation, "print(\"STOLEN\")"),
        )
        .await
        .unwrap_err();
        assert!(busy.contains("BUSY"), "a continuation must refuse: {busy}");
        f.child.send("print(\"CONTINUED\")\nend\n");
        let continued = f.child.until("CONTINUED\r\n");
        assert!(!continued.contains("STOLEN"), "{continued}");

        // A running evaluation. `sleep` holds the shell in a foreground child.
        let generation = prompt_generation(&mut f.parent, &f.bound).await;
        f.child.send("run_stream([\"sleep\", \"2\"])\n");
        tokio::time::sleep(Duration::from_millis(400)).await;
        let busy = execute_call(
            &mut f.parent,
            &f.bound,
            "shell.execute",
            execute_request(&f.bound, 3, generation, "print(\"STOLEN\")"),
        )
        .await
        .unwrap_err();
        assert!(
            busy.contains("BUSY") || busy.contains("STALE_GENERATION"),
            "a running evaluation must refuse: {busy}"
        );
        f.child.send("print(\"SLEPT\")\n");
        let slept = f.child.until("SLEPT\r\n");
        assert!(!slept.contains("STOLEN"), "{slept}");

        // A malformed or over-long submission never reaches the prompt at all.
        let generation = prompt_generation(&mut f.parent, &f.bound).await;
        let mut malformed = execute_request(&f.bound, 4, generation, "print(1)");
        malformed["detach"] = serde_json::json!(true);
        let error = execute_call(&mut f.parent, &f.bound, "shell.execute", malformed)
            .await
            .unwrap_err();
        assert!(error.contains("INVALID_REQUEST"), "{error}");
        let huge = execute_request(&f.bound, 5, generation, &"x".repeat(5000));
        let error = execute_call(&mut f.parent, &f.bound, "shell.execute", huge)
            .await
            .unwrap_err();
        assert!(error.contains("INVALID_REQUEST"), "{error}");

        // An unknown operation is an unknown outcome, not a guess.
        let error = execute_call(
            &mut f.parent,
            &f.bound,
            "shell.execute.result",
            operation_request(&f.bound, 9999),
        )
        .await
        .unwrap_err();
        assert!(error.contains("UNKNOWN_OUTCOME"), "{error}");
        teardown(f).await;
    });
}

/// Reads the current prompt generation without insisting the shell is idle
/// first: a continuation prompt is `prompt-ready` too, and the refusal under
/// test is the one that happens when the generation is RIGHT.
async fn prompt_generation_now(parent: &mut Parent, record: &SessionRecord) -> u64 {
    counter(&status(parent, record).await["status"]["snapshot"]["prompt_generation"])
}

/// J-8 on the live shell: a cancel resolves the exact operation, reports what
/// actually happened, and never reaches a successor.
#[test]
fn stage_d_cancellation_resolves_the_exact_operation() {
    let _fixture = fixture_guard();
    runtime().block_on(async {
        let mut f = stage_d_fixture("owned").await;
        let generation = prompt_generation(&mut f.parent, &f.bound).await;
        // A pure-Mix loop: cooperative cancellation at the evaluator's own
        // checkpoints is exactly the guarantee the table claims for this class.
        let accepted = execute_call(
            &mut f.parent,
            &f.bound,
            "shell.execute",
            execute_request(
                &f.bound,
                1,
                generation,
                "$i = 0\nwhile $i < 100000000\n  $i = $i + 1\ndone\nprint(\"LOOP_FINISHED\")",
            ),
        )
        .await
        .expect("admitted");
        let operation = counter(&accepted["operation_id"]);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let cancelled = execute_call(
            &mut f.parent,
            &f.bound,
            "shell.execute.cancel",
            operation_request(&f.bound, operation),
        )
        .await
        .expect("a live operation resolves");
        assert_eq!(cancelled["outcome"], "requested");
        assert!(
            cancelled["delivery"]
                .as_str()
                .unwrap()
                .contains("cooperative"),
            "delivery must not be described as a guarantee: {cancelled}"
        );
        let result = result_of(&mut f.parent, &f.bound, operation).await;
        assert_eq!(result["result"]["outcome"], "cancelled", "{result}");
        assert_eq!(result["result"]["cancellation"]["requested"], true);
        assert_eq!(result["result"]["cancellation"]["source"], "request");
        assert_eq!(result["result"]["cancellation"]["delivered"], "cooperative");

        // The same cancel arriving late answers the real outcome rather than
        // pretending, and a second evaluation is untouched by it.
        let late = execute_call(
            &mut f.parent,
            &f.bound,
            "shell.execute.cancel",
            operation_request(&f.bound, operation),
        )
        .await
        .expect("a finished operation still resolves");
        assert_eq!(late["outcome"], "already_finished");

        let generation = prompt_generation(&mut f.parent, &f.bound).await;
        let successor = execute_call(
            &mut f.parent,
            &f.bound,
            "shell.execute",
            execute_request(&f.bound, 2, generation, "print(\"SUCCESSOR_RAN\")"),
        )
        .await
        .expect("admitted");
        let successor = counter(&successor["operation_id"]);
        assert_ne!(successor, operation);
        let result = result_of(&mut f.parent, &f.bound, successor).await;
        assert_eq!(
            result["result"]["outcome"], "completed",
            "the old cancellation reached a successor: {result}"
        );
        assert_eq!(result["result"]["cancellation"]["requested"], false);
        let pane = f.child.until("SUCCESSOR_RAN\r\n");
        // The echo reproduces the submitted source, so the marker appears there
        // by construction. What must not appear is the marker as OUTPUT — the
        // line the print would have written, terminated by the pane's CRLF.
        assert!(
            !pane.contains("LOOP_FINISHED\r\n"),
            "the cancelled loop ran to completion: {pane}"
        );

        // Cancelling something this shell never admitted addresses nothing.
        let error = execute_call(
            &mut f.parent,
            &f.bound,
            "shell.execute.cancel",
            operation_request(&f.bound, 9999),
        )
        .await
        .unwrap_err();
        assert!(error.contains("UNKNOWN_OUTCOME"), "{error}");
        teardown(f).await;
    });
}

/// A SIGINT delivered while an admitted execution is running belongs to THAT
/// evaluation. The bug this refuses is the one the single global flag made
/// inevitable: the interrupt surviving into whatever ran next.
#[test]
fn stage_d_a_signal_during_an_admitted_execution_does_not_reach_the_next_one() {
    let _fixture = fixture_guard();
    runtime().block_on(async {
        let mut f = stage_d_fixture("owned").await;
        let generation = prompt_generation(&mut f.parent, &f.bound).await;
        let accepted = execute_call(
            &mut f.parent,
            &f.bound,
            "shell.execute",
            execute_request(
                &f.bound,
                1,
                generation,
                "$i = 0\nwhile $i < 100000000\n  $i = $i + 1\ndone\nprint(\"LOOP_FINISHED\")",
            ),
        )
        .await
        .expect("admitted");
        let operation = counter(&accepted["operation_id"]);
        tokio::time::sleep(Duration::from_millis(400)).await;
        // The shell is in cooked mode running the admitted evaluation, so a
        // real SIGINT to the process is the same thing a Ctrl-C would be.
        unsafe {
            libc::kill(f.child.pid(), libc::SIGINT);
        }
        let result = result_of(&mut f.parent, &f.bound, operation).await;
        assert_eq!(result["result"]["cancellation"]["source"], "signal", "{result}");
        assert_eq!(result["result"]["outcome"], "cancelled", "{result}");

        // The very next line must run normally. Before the per-evaluation
        // mapping, a late-consumed interrupt tripped exactly here.
        let generation = prompt_generation(&mut f.parent, &f.bound).await;
        let next = execute_call(
            &mut f.parent,
            &f.bound,
            "shell.execute",
            execute_request(&f.bound, 2, generation, "print(\"AFTER_SIGNAL\")"),
        )
        .await
        .expect("admitted");
        let next = counter(&next["operation_id"]);
        let result = result_of(&mut f.parent, &f.bound, next).await;
        assert_eq!(
            result["result"]["outcome"], "completed",
            "the signal reached the next evaluation: {result}"
        );
        assert_eq!(result["result"]["cancellation"]["requested"], false);
        f.child.until("AFTER_SIGNAL\r\n");
        teardown(f).await;
    });
}

/// Under rustyline the terminal cannot be released without a keypress. That is
/// a declared limitation, and UNSUPPORTED is the honest answer — a BUSY would
/// invite a retry that could never succeed.
#[test]
fn stage_d_is_unsupported_rather_than_busy_without_the_owned_editor() {
    let _fixture = fixture_guard();
    runtime().block_on(async {
        let mut f = stage_d_fixture("legacy").await;
        let generation = prompt_generation(&mut f.parent, &f.bound).await;
        let error = execute_call(
            &mut f.parent,
            &f.bound,
            "shell.execute",
            execute_request(&f.bound, 1, generation, "print(\"NEVER\")"),
        )
        .await
        .unwrap_err();
        assert!(error.contains("UNSUPPORTED"), "{error}");
        assert!(!error.contains("BUSY"), "{error}");
        // And the capability report agrees with the refusal, so a caller can
        // find out without submitting anything.
        let view = status(&mut f.parent, &f.bound).await;
        assert_eq!(view["capabilities"]["evaluation_submit"], "UNSUPPORTED");
        assert_eq!(view["capabilities"]["evaluation_inspect"], "UNSUPPORTED");
        teardown(f).await;
    });
}

/// The capability report under the owned editor says what the surface actually
/// does, and a caller without `execute` authority is refused before learning
/// anything about it.
#[test]
fn stage_d_reports_its_own_capability_and_refuses_unauthorised_callers() {
    let _fixture = fixture_guard();
    runtime().block_on(async {
        let mut f = stage_d_fixture("owned").await;
        let view = status(&mut f.parent, &f.bound).await;
        assert_eq!(
            view["capabilities"]["evaluation_submit"],
            "idle-prompt-admission"
        );
        assert_eq!(view["capabilities"]["evaluation_inspect"], "result-and-cancel");
        let generation = prompt_generation(&mut f.parent, &f.bound).await;
        // An unrelated Term instance holds every capability on its OWN records
        // and none on this one.
        let foreign = Parent::new(&f.broker).await;
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            foreign.connection.client().call(
                &f.bound.name,
                "shell.execute",
                execute_request(&f.bound, 1, generation, "print(\"STOLEN\")"),
            ),
        )
        .await
        .expect("a denial must reply, not time out")
        .unwrap_err()
        .to_string();
        assert!(error.contains("REFUSED"), "{error}");
        let tcp = NodedClient::connect_anonymous(&f.broker.url).await.unwrap();
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            tcp.call(
                &f.bound.name,
                "shell.execute",
                execute_request(&f.bound, 2, generation, "print(\"STOLEN\")"),
            ),
        )
        .await
        .expect("an unverified denial must reply, not time out")
        .unwrap_err()
        .to_string();
        assert!(error.contains("REFUSED"), "{error}");
        tcp.close().await;

        // An unauthorised caller must not be able to tell a verb that EXISTS
        // from one that does not. If a known verb answered REFUSED and an
        // unknown one UNSUPPORTED, the refusal would itself be a probe of the
        // verb table — so both answer the same, and the distinction is only
        // ever visible to a caller that was admitted.
        for verb in [
            "shell.status",
            "shell.execute",
            "shell.execute.result",
            "shell.execute.cancel",
            "shell.does.not.exist",
        ] {
            let error = tokio::time::timeout(
                Duration::from_secs(5),
                foreign.connection.client().call(
                    &f.bound.name,
                    verb,
                    execute_request(&f.bound, 3, generation, "print(1)"),
                ),
            )
            .await
            .expect("a denial must reply, not time out")
            .unwrap_err()
            .to_string();
            assert_eq!(error, r#"{"error_code":"REFUSED"}"#, "{verb} leaked: {error}");
        }

        f.child.send("print(\"SWEEP_DONE\")\n");
        let pane = f.child.until("SWEEP_DONE\r\n");
        assert!(!pane.contains("STOLEN"), "{pane}");
        foreign.revoke_and_verify(&f.broker).await;
        teardown(f).await;
    });
}
