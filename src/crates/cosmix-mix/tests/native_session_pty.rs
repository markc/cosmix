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
        Self::spawn_with(broker, launch, editor, &[])
    }
    fn spawn_with(
        broker: &Broker,
        launch: &LaunchFd,
        editor: &str,
        extra: &[(String, String)],
    ) -> Self {
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
        let mut env = env;
        env.extend_from_slice(extra);
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
        for feature in ["jobs", "job_signal", "foreground", "input", "events"] {
            assert_eq!(idle["capabilities"][feature], "UNSUPPORTED");
        }
        // P4 turned isolated tasks on, and unlike evaluation submit they do
        // NOT depend on the editor: a task is a separate process that never
        // touches the prompt, so both editors report it the same way.
        assert_eq!(
            idle["capabilities"]["isolated_task"],
            "supervised-process; poll-only (no watch/list)"
        );
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

async fn stage_d_fixture_with(editor: &str, extra: &[(String, String)]) -> Fixture {
    let broker = Broker::start();
    let mut parent = Parent::new(&broker).await;
    let key = fresh_key().unwrap();
    let grant = parent
        .grant(HexBytes(key.verifying_key().to_bytes()), 1)
        .await;
    let launch = LaunchFd::new(&grant, &key).unwrap();
    let mut child = Child::spawn_with(&broker, &launch, editor, extra);
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

/// THE BLOCKER. A bounded wait gives up on the reply channel, not on the work:
/// the envelope is still queued and will be processed. The failure this refuses
/// is the one where the shell tells the caller nothing happened, executes
/// anyway, and then runs the line a SECOND time on the caller's retry.
#[test]
fn stage_d_an_abandoned_admission_never_executes_and_its_retry_does_not_re_run() {
    let _fixture = fixture_guard();
    runtime().block_on(async {
        // Stall the editor past the admit budget, so the owner gives up while
        // the envelope is still queued — the one interleaving timing alone
        // cannot produce.
        let mut f = stage_d_fixture_with(
            "owned",
            &[("MIX_ADMIT_DELAY_MS".into(), "2500".into())],
        )
        .await;
        let generation = prompt_generation(&mut f.parent, &f.bound).await;
        let submission =
            execute_request(&f.bound, 1, generation, "print(\"MUST_NOT_RUN\")");
        let error = execute_call(
            &mut f.parent,
            &f.bound,
            "shell.execute",
            submission.clone(),
        )
        .await
        .unwrap_err();
        // Never BUSY: a caller told BUSY retries, and a retry of something that
        // might have executed is how the line runs twice.
        assert!(error.contains("UNKNOWN_OUTCOME"), "{error}");
        assert!(!error.contains("BUSY"), "{error}");
        let refusal: serde_json::Value = serde_json::from_str(&error).unwrap();
        let operation = counter(&refusal["operation_id"]);

        // The record stays RESOLVABLE. Deleting it was what made the retry a
        // fresh submission instead of a replay.
        let result = result_of(&mut f.parent, &f.bound, operation).await;
        assert_eq!(result["result"]["outcome"], "not_started", "{result}");

        // The byte-identical retry replays that outcome. It must not execute.
        let retry = execute_call(&mut f.parent, &f.bound, "shell.execute", submission)
            .await
            .unwrap_err();
        assert!(retry.contains("UNKNOWN_OUTCOME"), "{retry}");

        // And the pane is the real proof: no announcement, no output, ever —
        // including after the editor finally processes the stale envelope.
        tokio::time::sleep(Duration::from_secs(4)).await;
        f.child.send("print(\"SWEEP_DONE\")\n");
        let pane = f.child.until("SWEEP_DONE\r\n");
        assert!(
            !pane.contains("MUST_NOT_RUN"),
            "an abandoned admission executed: {pane}"
        );
        assert!(
            !pane.contains("admitted for"),
            "an abandoned admission still marked the pane: {pane}"
        );
        teardown(f).await;
    });
}

/// §8's human-first rule has to hold through the reservation window too. The
/// window is cooked mode with kernel echo and, before the fix, an unpolled tty:
/// a keystroke landing there was echoed into the announcement line and then
/// handed to the admitted execution as stdin.
#[test]
fn stage_d_a_keystroke_during_the_reservation_refuses_the_admission_intact() {
    let _fixture = fixture_guard();
    runtime().block_on(async {
        // Long enough that the human types while the reservation stands.
        let mut f = stage_d_fixture_with(
            "owned",
            &[("MIX_RESERVE_HOLD_MS".into(), "1200".into())],
        )
        .await;
        let generation = prompt_generation(&mut f.parent, &f.bound).await;
        let submitting = tokio::spawn({
            let name = f.bound.name.clone();
            let client = f.parent.connection.clone();
            let body = execute_request(&f.bound, 1, generation, "print(\"STOLEN\")");
            async move {
                client
                    .client()
                    .call(&name, "shell.execute", body)
                    .await
                    .map_err(|e| e.to_string())
            }
        });
        // A SINGLE BYTE, mid-reservation — not a completed line. §8 puts the
        // cooked-mode restore at the commit, so the window is still RAW: the
        // byte is readable immediately, goes through the ordinary key path and
        // ends the reservation on its way. Under the old placement this byte
        // sat invisible in a canonical line discipline, was kernel-echoed into
        // the announcement, and became the admitted execution's stdin.
        tokio::time::sleep(Duration::from_millis(200)).await;
        f.child.send("p");
        let outcome = tokio::time::timeout(Duration::from_secs(10), submitting)
            .await
            .expect("the submission must answer")
            .unwrap();
        // BUSY, pinned exactly: the prompt is still the human's, still on the
        // same generation, and they are mid-draft. Nothing was announced.
        let error = outcome.expect_err("a keystroke must refuse the admission");
        assert_eq!(error, r#"{"error_code":"BUSY"}"#, "{error}");

        // The draft is intact and still editable — the byte is the first
        // character of the line the human goes on to finish.
        f.child.send("rint(\"HUMAN_WINS\")\n");
        let pane = f.child.until("HUMAN_WINS\r\n");
        assert!(!pane.contains("STOLEN"), "the agent's line ran: {pane}");
        assert!(
            !pane.contains("admitted for"),
            "a refused admission announced itself: {pane}"
        );

        // And the completed-line case still refuses, with its own exact code:
        // the human's line consumed the generation the caller named.
        let generation = prompt_generation(&mut f.parent, &f.bound).await;
        let submitting = tokio::spawn({
            let name = f.bound.name.clone();
            let client = f.parent.connection.clone();
            let body = execute_request(&f.bound, 2, generation, "print(\"STOLEN\")");
            async move {
                client
                    .client()
                    .call(&name, "shell.execute", body)
                    .await
                    .map_err(|e| e.to_string())
            }
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        f.child.send("print(\"HUMAN_AGAIN\")\n");
        let error = tokio::time::timeout(Duration::from_secs(10), submitting)
            .await
            .expect("the submission must answer")
            .unwrap()
            .expect_err("a completed human line must refuse the admission");
        assert!(
            error == r#"{"error_code":"BUSY"}"#
                || error == r#"{"error_code":"STALE_GENERATION"}"#,
            "a completed line must refuse as BUSY (caught at the reservation) \
             or STALE_GENERATION (caught at the recheck), not {error}"
        );
        let pane = f.child.until("HUMAN_AGAIN\r\n");
        assert!(!pane.contains("STOLEN"), "the agent's line ran: {pane}");
        teardown(f).await;
    });
}

/// The guarantee table's managed-children row, exercised rather than asserted.
/// A `sleep` polls nothing, so a cooperative flag alone would never reach it —
/// only the group signal does.
#[test]
fn stage_d_cancelling_a_managed_foreground_job_signals_its_process_group() {
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
                "sleep 30",
            ),
        )
        .await
        .expect("admitted");
        let operation = counter(&accepted["operation_id"]);
        tokio::time::sleep(Duration::from_millis(600)).await;
        let started = Instant::now();
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
            cancelled["signalled_pgid"].is_string() || cancelled["signalled_pgid"].is_number(),
            "the managed foreground job was not signalled: {cancelled}"
        );
        // The TIMING is the assertion, and it is not vacuous: `sleep 30` polls
        // nothing, so a cancellation that only set a cooperative flag would
        // leave it running and `result_of` would hit its own deadline long
        // before this line. Arriving at all is what proves the group signal
        // landed on something that could not have noticed a flag.
        let result = result_of(&mut f.parent, &f.bound, operation).await;
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "cancellation did not reach the job group"
        );
        assert_eq!(result["result"]["outcome"], "cancelled", "{result}");
        assert_eq!(result["result"]["cancellation"]["delivered"], "cooperative");
        teardown(f).await;
    });
}

/// A captured runner reports interruption as an `Ok` result carrying a flag,
/// not as an error, so a report derived from error prose called this
/// `completed`. Whether a cancellation landed is a fact the machinery records.
#[test]
fn stage_d_a_cancelled_captured_runner_reports_cancelled_not_completed() {
    let _fixture = fixture_guard();
    runtime().block_on(async {
        let mut f = stage_d_fixture("owned").await;
        for (id, source) in [
            (1u64, "$r = run_argv([\"sleep\", \"20\"])\nprint(\"RAN_THROUGH\")"),
            (2, "$r = run(\"sleep 20\")\nprint(\"RAN_THROUGH\")"),
        ] {
            let generation = prompt_generation(&mut f.parent, &f.bound).await;
            let accepted = execute_call(
                &mut f.parent,
                &f.bound,
                "shell.execute",
                execute_request(&f.bound, id, generation, source),
            )
            .await
            .unwrap_or_else(|e| panic!("admitted: {e}"));
            let operation = counter(&accepted["operation_id"]);
            tokio::time::sleep(Duration::from_millis(500)).await;
            execute_call(
                &mut f.parent,
                &f.bound,
                "shell.execute.cancel",
                operation_request(&f.bound, operation),
            )
            .await
            .expect("cancel resolves");
            let result = result_of(&mut f.parent, &f.bound, operation).await;
            assert_eq!(
                result["result"]["cancellation"]["delivered"], "cooperative",
                "{source}: {result}"
            );
            assert_eq!(result["result"]["outcome"], "cancelled", "{source}: {result}");
        }
        teardown(f).await;
    });
}

/// Drafts that are not plain text are drafts too. Each of these refuses with
/// the EXACT code — pinned, not an or-of-two — and leaves the human's state
/// byte-intact.
#[test]
fn stage_d_paste_and_search_drafts_are_preserved_with_exact_refusals() {
    let _fixture = fixture_guard();
    runtime().block_on(async {
        let mut f = stage_d_fixture("owned").await;

        // A bracketed paste in progress.
        let generation = prompt_generation(&mut f.parent, &f.bound).await;
        f.child.send("\x1b[200~print(\"PASTED");
        tokio::time::sleep(Duration::from_millis(300)).await;
        let error = execute_call(
            &mut f.parent,
            &f.bound,
            "shell.execute",
            execute_request(&f.bound, 1, generation, "print(\"STOLEN\")"),
        )
        .await
        .unwrap_err();
        assert_eq!(error, r#"{"error_code":"BUSY"}"#, "paste: {error}");
        f.child.send("_OK\")\x1b[201~\n");
        let pane = f.child.until("PASTED_OK\r\n");
        assert!(!pane.contains("STOLEN"), "{pane}");

        // A reverse history search in progress.
        let generation = prompt_generation(&mut f.parent, &f.bound).await;
        f.child.send("\x12PASTED");
        tokio::time::sleep(Duration::from_millis(300)).await;
        let error = execute_call(
            &mut f.parent,
            &f.bound,
            "shell.execute",
            execute_request(&f.bound, 2, generation, "print(\"STOLEN\")"),
        )
        .await
        .unwrap_err();
        assert_eq!(error, r#"{"error_code":"BUSY"}"#, "search: {error}");
        // Leaving the search restores the recalled line, which still runs.
        f.child.send("\x07\n");
        tokio::time::sleep(Duration::from_millis(300)).await;
        f.child.send("print(\"SEARCH_DONE\")\n");
        let pane = f.child.until("SEARCH_DONE\r\n");
        assert!(!pane.contains("STOLEN"), "{pane}");

        // And the empty/whitespace submission, which would otherwise be
        // announced on the pane and burn a generation to run nothing.
        let generation = prompt_generation(&mut f.parent, &f.bound).await;
        for blank in ["", "   ", "\n\t "] {
            let error = execute_call(
                &mut f.parent,
                &f.bound,
                "shell.execute",
                execute_request(&f.bound, 3, generation, blank),
            )
            .await
            .unwrap_err();
            assert_eq!(error, r#"{"error_code":"INVALID_REQUEST"}"#, "{blank:?}");
        }
        assert_eq!(
            prompt_generation(&mut f.parent, &f.bound).await,
            generation,
            "a refused submission must not consume a prompt generation"
        );
        teardown(f).await;
    });
}

/// An admission owner that dies mid-sequence must not leave a human without a
/// prompt. The editor's own reservation deadline is the backstop, and this is
/// the only thing that exercises it.
#[test]
fn stage_d_an_abandoned_reservation_returns_the_prompt_to_the_human() {
    let _fixture = fixture_guard();
    runtime().block_on(async {
        let mut f = stage_d_fixture_with(
            "owned",
            &[("MIX_ADMIT_DELAY_MS".into(), "2500".into())],
        )
        .await;
        let generation = prompt_generation(&mut f.parent, &f.bound).await;
        let _ = execute_call(
            &mut f.parent,
            &f.bound,
            "shell.execute",
            execute_request(&f.bound, 1, generation, "print(\"NEVER\")"),
        )
        .await
        .unwrap_err();
        // The prompt comes back on its own, with no keystroke to prod it, and
        // the shell is fully usable afterwards.
        let recovered = phase(&mut f.parent, &f.bound, "prompt-ready").await;
        assert_eq!(recovered["status"]["snapshot"]["continuation"], false);
        f.child.send("print(\"HUMAN_AGAIN\")\n");
        let pane = f.child.until("HUMAN_AGAIN\r\n");
        assert!(!pane.contains("NEVER"), "{pane}");
        teardown(f).await;
    });
}

/// F1, end to end: the refusal the D11 design makes COMMONEST must not burn the
/// caller's request id. Before this, `store.admit` spent the id before anything
/// was tried, so a keystroke-refused submission answered BUSY ("simply retry")
/// and every retry of that id then met a retired mark, permanently.
#[test]
fn stage_d_a_refused_submission_may_be_retried_under_the_same_id() {
    let _fixture = fixture_guard();
    runtime().block_on(async {
        let mut f = stage_d_fixture_with(
            "owned",
            &[("MIX_RESERVE_HOLD_MS".into(), "1200".into())],
        )
        .await;
        let generation = prompt_generation(&mut f.parent, &f.bound).await;
        let body = execute_request(&f.bound, 1, generation, "print(\"RETRY_RAN\")");
        let submitting = tokio::spawn({
            let name = f.bound.name.clone();
            let client = f.parent.connection.clone();
            let body = body.clone();
            async move {
                client
                    .client()
                    .call(&name, "shell.execute", body)
                    .await
                    .map_err(|e| e.to_string())
            }
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        f.child.send("x");
        let error = tokio::time::timeout(Duration::from_secs(10), submitting)
            .await
            .expect("the submission must answer")
            .unwrap()
            .expect_err("a keystroke must refuse the admission");
        assert_eq!(error, r#"{"error_code":"BUSY"}"#, "{error}");

        // Clear the stray byte, then retry THE SAME request id. The contract
        // that refusal states is that this is a real submission, not a replay.
        f.child.send("\x08\n");
        let generation = prompt_generation(&mut f.parent, &f.bound).await;
        let retry = execute_request(&f.bound, 1, generation, "print(\"RETRY_RAN\")");
        let accepted = execute_call(&mut f.parent, &f.bound, "shell.execute", retry)
            .await
            .unwrap_or_else(|e| {
                panic!("the refused id was burned; retry answered {e}")
            });
        assert_eq!(accepted["status"], "accepted", "{accepted}");
        let operation = counter(&accepted["operation_id"]);
        let result = result_of(&mut f.parent, &f.bound, operation).await;
        assert_eq!(result["result"]["outcome"], "completed", "{result}");
        f.child.until("RETRY_RAN\r\n");
        teardown(f).await;
    });
}

/// The `Unknown` branch: the owner's abandon LOSES, so the editor is already
/// committed and the outcome is genuinely undetermined. The contract is that
/// the record stays resolvable and the real outcome lands in it — an
/// UNKNOWN_OUTCOME that resolved to nothing would be a permanent lie.
#[test]
fn stage_d_an_undetermined_admission_still_resolves_to_its_real_outcome() {
    let _fixture = fixture_guard();
    runtime().block_on(async {
        // Stall AFTER the claim, past budget + grace, so abandon loses.
        let mut f = stage_d_fixture_with(
            "owned",
            &[("MIX_CLAIM_DELAY_MS".into(), "2200".into())],
        )
        .await;
        let generation = prompt_generation(&mut f.parent, &f.bound).await;
        let error = execute_call(
            &mut f.parent,
            &f.bound,
            "shell.execute",
            execute_request(&f.bound, 1, generation, "print(\"UNDETERMINED_RAN\")"),
        )
        .await
        .unwrap_err();
        assert!(error.contains("UNKNOWN_OUTCOME"), "{error}");
        let refusal: serde_json::Value = serde_json::from_str(&error).unwrap();
        assert_eq!(
            refusal["reason"], "admission_claimed_without_report",
            "the caller cannot tell this from the abandoned case: {refusal}"
        );
        let operation = counter(&refusal["operation_id"]);

        // The editor DID go on to execute it. The record must carry that.
        let result = result_of(&mut f.parent, &f.bound, operation).await;
        assert_eq!(result["result"]["outcome"], "completed", "{result}");
        f.child.until("UNDETERMINED_RAN\r\n");
        teardown(f).await;
    });
}

// ---------------------------------------------------------------------------
// P4: isolated supervised tasks.
//
// A task is a separate process, so unlike stage D these fixtures are not about
// the prompt. What they prove is the opposite: that the task got NOTHING it was
// not given, that termination is hard rather than cooperative, and that the
// outcome comes from wait() rather than from having sent a signal.
//
// Four fixtures, not nine. Each `stage_d_fixture` starts a real broker and a
// real enrolled child, and this suite is run NESTED inside the desktop test
// suite, in parallel with its own brokers and PTYs. Nine spawns of that weight
// measurably starved `status_flood_preserves_lease_and_restart_ack` — a
// fixture that must hold a 15-second lease under a 64-way flood — into losing
// its attachment. Grouping by theme keeps every assertion and costs four.

fn task_request(
    record: &SessionRecord,
    request_id: u64,
    body: serde_json::Value,
) -> serde_json::Value {
    let mut request = serde_json::json!({
        "version": 1,
        "target": status_request(record)["target"],
        "request_id": request_id.to_string(),
        "cwd": "/tmp",
        "env": [],
        "timeout_ms": "10000",
    });
    for (key, value) in body.as_object().unwrap() {
        request[key] = value.clone();
    }
    request
}

async fn task_report(
    parent: &mut Parent,
    record: &SessionRecord,
    operation: u64,
) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let value = execute_call(
            parent,
            record,
            "shell.task.result",
            operation_request(record, operation),
        )
        .await
        .expect("a known task always has a state");
        if value["state"] == "settled" {
            return value;
        }
        assert!(
            value["state"] == "running" || value["state"] == "cancelling",
            "unexpected task state: {value}"
        );
        assert!(Instant::now() < deadline, "task never settled: {value}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Request ids must ASCEND within a fixture. The shell retires anything below
/// its high-water mark, so inserting a case with a lower id than one already
/// used makes the LATER submission fail with UNKNOWN_OUTCOME — a confusing way
/// to discover you edited in the wrong place. A refused submission does not
/// advance the mark, which is why the deliberately-invalid ids can sit out of
/// line.
async fn submit_task(
    parent: &mut Parent,
    record: &SessionRecord,
    request_id: u64,
    body: serde_json::Value,
) -> Result<u64, String> {
    let accepted = execute_call(
        parent,
        record,
        "shell.task.submit",
        task_request(record, request_id, body),
    )
    .await?;
    assert_eq!(accepted["status"], "accepted", "{accepted}");
    Ok(counter(&accepted["operation_id"]))
}

/// Both modes end to end, and the kind tag that keeps the two surfaces apart.
#[test]
fn p4_both_modes_report_accurately_and_the_surfaces_stay_separate() {
    let _fixture = fixture_guard();
    runtime().block_on(async {
        let mut f = stage_d_fixture("owned").await;

        // SOURCE mode: the typed value travels the result descriptor while the
        // program's text goes to stdout, and the two never mix.
        let operation = submit_task(
            &mut f.parent,
            &f.bound,
            1,
            serde_json::json!({"source": "print(\"ON_STDOUT\")\neprint(\"ON_STDERR\")\n6*7"}),
        )
        .await
        .expect("a task is admitted regardless of what the prompt is doing");
        let report = task_report(&mut f.parent, &f.bound, operation).await;
        let task = &report["report"];
        assert_eq!(task["outcome"]["kind"], "exited", "{task}");
        assert_eq!(task["outcome"]["code"], 0, "{task}");
        // Attribution is exact BY CONSTRUCTION here — separate pipes, which is
        // the whole reason the manual points at this mode.
        assert!(task["stdout"]["text"].as_str().unwrap().contains("ON_STDOUT"));
        assert!(task["stderr"]["text"].as_str().unwrap().contains("ON_STDERR"));
        assert!(!task["stdout"]["text"].as_str().unwrap().contains("ON_STDERR"));
        assert_eq!(task["result"]["kind"], "value", "{task}");
        let data = task["result"]["data"].as_str().unwrap();
        assert!(data.contains("\"ok\"") && data.contains("42"), "{data}");
        assert!(
            !task["stdout"]["text"].as_str().unwrap().contains("42"),
            "the value leaked into the text stream: {task}"
        );

        // ARGV mode: real exit status, and no structured value rather than an
        // absent one presented as a failure.
        let operation = submit_task(
            &mut f.parent,
            &f.bound,
            2,
            serde_json::json!({"argv": ["sh", "-c", "echo OUT; echo ERR >&2; exit 3"]}),
        )
        .await
        .expect("admitted");
        let report = task_report(&mut f.parent, &f.bound, operation).await;
        let task = &report["report"];
        assert_eq!(task["outcome"]["code"], 3, "{task}");
        assert!(task["stdout"]["text"].as_str().unwrap().contains("OUT"));
        assert!(task["stderr"]["text"].as_str().unwrap().contains("ERR"));
        assert_eq!(task["result"]["kind"], "not_applicable", "{task}");

        // The union admits exactly one side.
        for body in [
            serde_json::json!({"source": "1", "argv": ["true"]}),
            serde_json::json!({}),
        ] {
            let error = submit_task(&mut f.parent, &f.bound, 9, body)
                .await
                .unwrap_err();
            assert!(error.contains("INVALID_ARGUMENT"), "{error}");
        }

        // ONE dedupe space, records tagged: an identical retry replays.
        let body = serde_json::json!({"source": "print(\"TASK_ONCE\")\n1"});
        let first = submit_task(&mut f.parent, &f.bound, 3, body.clone())
            .await
            .expect("admitted");
        task_report(&mut f.parent, &f.bound, first).await;
        let again = submit_task(&mut f.parent, &f.bound, 3, body)
            .await
            .expect("a retry replays");
        assert_eq!(again, first, "a retry minted a second task");

        // Neither surface can read the other's operations, and the refusal is
        // the same one an unknown id gets — no oracle either way.
        let error = execute_call(
            &mut f.parent,
            &f.bound,
            "shell.execute.result",
            operation_request(&f.bound, first),
        )
        .await
        .unwrap_err();
        assert!(error.contains("UNKNOWN_OUTCOME"), "{error}");
        let generation = prompt_generation(&mut f.parent, &f.bound).await;
        let accepted = execute_call(
            &mut f.parent,
            &f.bound,
            "shell.execute",
            execute_request(&f.bound, 4, generation, "print(\"EVAL\")"),
        )
        .await
        .expect("admitted");
        let evaluation = counter(&accepted["operation_id"]);
        let error = execute_call(
            &mut f.parent,
            &f.bound,
            "shell.task.result",
            operation_request(&f.bound, evaluation),
        )
        .await
        .unwrap_err();
        assert!(error.contains("UNKNOWN_OUTCOME"), "{error}");
        teardown(f).await;
    });
}

/// A task inherits NOTHING it was not given — asserted as whole-set equality,
/// because spot-checking a few names cannot show the absence of the rest.
#[test]
fn p4_a_task_inherits_only_what_it_was_given() {
    let _fixture = fixture_guard();
    runtime().block_on(async {
        let mut f = stage_d_fixture("owned").await;
        // A variable in the SHELL's live environment must not reach the task:
        // the base set is snapshotted at startup and enumerated, not inherited.
        f.child
            .send("$x = setenv(\"P4_SHELL_SECRET\", \"leaked\")\nprint(\"ENV_SET\")\n");
        f.child.until("ENV_SET\r\n");
        let operation = submit_task(
            &mut f.parent,
            &f.bound,
            1,
            serde_json::json!({
                "source": "print(read_file(\"/proc/self/environ\"))",
                "env": [["P4_OVERLAY", "given"], ["PATH", "/p4/overridden"]],
            }),
        )
        .await
        .expect("admitted");
        let report = task_report(&mut f.parent, &f.bound, operation).await;
        let environ = report["report"]["stdout"]["text"].as_str().unwrap();
        let names: std::collections::BTreeSet<&str> = environ
            .split('\0')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .filter_map(|entry| entry.split('=').next())
            .collect();
        assert!(
            !names.contains("P4_SHELL_SECRET"),
            "a live shell variable reached a task: {names:?}"
        );
        let allowed: std::collections::BTreeSet<&str> = [
            "HOME", "USER", "PATH", "LANG", "TERM", "COSMIX", "COSMIX_SRC",
            "COSMIX_BIN", "COSMIX_ETC", "COSMIX_NODE_CONFIG",
            "COSMIX_BROKER_ACCOUNT", "P4_OVERLAY",
        ]
        .into_iter()
        .collect();
        let extra: Vec<_> = names.difference(&allowed).collect();
        assert!(extra.is_empty(), "undeclared inheritance: {extra:?}");
        assert!(names.contains("P4_OVERLAY") && names.contains("TERM"), "{names:?}");
        // PRESENCE, not just absence. A subset check passes happily when the
        // base set shrinks, so dropping PATH from BASE_NAMES would leave every
        // task unable to find a program and no fixture would notice. Only names
        // the shell itself reliably has: LANG is absent on a headless build
        // worker, and asserting it would make this a test of the environment
        // rather than of the base set.
        for required in ["HOME", "USER", "PATH"] {
            assert!(
                names.contains(required),
                "{required} is missing from the base environment: {names:?}"
            );
        }
        // The overlay WINS over the base on a collision.
        assert!(
            environ.contains("PATH=/p4/overridden"),
            "the overlay did not win: {environ}"
        );

        // Descriptor hygiene, in ARGV mode because that is where it is
        // OBSERVABLE: a Mix interpreter opens its own descriptors at startup,
        // so a source task cannot distinguish inheritance from its own work.
        // `sh` opens nothing, so what it sees is what the spawn handed it —
        // and argv mode has no result channel, so the expected set is the three
        // standard streams plus the listing's own handle.
        let operation = submit_task(
            &mut f.parent,
            &f.bound,
            2,
            serde_json::json!({"argv": ["sh", "-c", "ls /proc/self/fd"]}),
        )
        .await
        .expect("admitted");
        let report = task_report(&mut f.parent, &f.bound, operation).await;
        let listed = report["report"]["stdout"]["text"].as_str().unwrap().trim();
        let mut open: Vec<i32> = listed
            .split_whitespace()
            .filter_map(|entry| entry.parse().ok())
            .collect();
        open.sort_unstable();
        open.dedup();
        for expected in [0, 1, 2] {
            assert!(open.contains(&expected), "fd {expected} missing: {listed}");
        }
        assert!(open.len() <= 4, "a descriptor leaked into the task: {listed}");

        // And in SOURCE mode the result channel must be PRESENT. Mix can list
        // its own descriptors, so the positive half of the contract — fd 3 is
        // the result channel, not an accident — is observable after all, even
        // though the interpreter's own startup descriptors make the negative
        // half unmeasurable here.
        let operation = submit_task(
            &mut f.parent,
            &f.bound,
            4,
            serde_json::json!({"source": "print(join(glob(\"/proc/self/fd/*\"), \" \"))"}),
        )
        .await
        .expect("admitted");
        let report = task_report(&mut f.parent, &f.bound, operation).await;
        let listed = report["report"]["stdout"]["text"].as_str().unwrap();
        let open: std::collections::BTreeSet<&str> = listed
            .split_whitespace()
            .filter_map(|path| path.rsplit('/').next())
            .collect();
        for expected in ["0", "1", "2", "3"] {
            assert!(
                open.contains(expected),
                "fd {expected} missing from a source task: {listed}"
            );
        }

        // stdin is /dev/null, not the pane: a reader sees EOF at once.
        let operation = submit_task(
            &mut f.parent,
            &f.bound,
            5,
            serde_json::json!({"argv": ["sh", "-c", "cat; echo DRAINED"]}),
        )
        .await
        .expect("admitted");
        let report = task_report(&mut f.parent, &f.bound, operation).await;
        assert!(
            report["report"]["stdout"]["text"]
                .as_str()
                .unwrap()
                .contains("DRAINED"),
            "stdin did not read EOF immediately: {report}"
        );
        teardown(f).await;
    });
}

/// The arc's ONE hard termination guarantee, both ways in.
#[test]
fn p4_termination_is_hard_and_the_outcome_names_the_policy() {
    let _fixture = fixture_guard();
    runtime().block_on(async {
        let mut f = stage_d_fixture("owned").await;

        // A timeout is POLICY. It must not be reported as an indistinguishable
        // external signal, and it must actually terminate the group.
        let started = Instant::now();
        let operation = submit_task(
            &mut f.parent,
            &f.bound,
            1,
            serde_json::json!({"argv": ["sleep", "30"], "timeout_ms": "800"}),
        )
        .await
        .expect("admitted");
        let report = task_report(&mut f.parent, &f.bound, operation).await;
        assert!(
            started.elapsed() < Duration::from_secs(25),
            "the timeout did not terminate the task"
        );
        let task = &report["report"];
        assert_eq!(task["outcome"]["kind"], "timeout", "{task}");
        assert_eq!(task["outcome"]["escalated_to"], "sigterm", "{task}");
        // The policy NAMES the outcome, but the wait status is carried beside
        // it rather than replaced by it — a caller must still be able to see
        // what the kernel actually reported.
        assert_eq!(task["outcome"]["wait"]["reaped"], true, "{task}");
        assert_eq!(
            task["outcome"]["wait"]["signal"], 15,
            "the wait facts must survive the policy name: {task}"
        );

        // A child that IGNORES SIGTERM is killed, and the report says how far
        // the ladder had to go — read from wait(), not from the signal call.
        let operation = submit_task(
            &mut f.parent,
            &f.bound,
            2,
            serde_json::json!({
                "argv": ["sh", "-c", "trap '' TERM; echo READY; while true; do sleep 1; done"],
                "timeout_ms": "15000",
            }),
        )
        .await
        .expect("admitted");
        tokio::time::sleep(Duration::from_millis(500)).await;
        let cancelled = execute_call(
            &mut f.parent,
            &f.bound,
            "shell.task.cancel",
            operation_request(&f.bound, operation),
        )
        .await
        .expect("a live task resolves");
        assert_eq!(cancelled["outcome"], "requested");
        let started = Instant::now();
        let report = task_report(&mut f.parent, &f.bound, operation).await;
        let task = &report["report"];
        assert_eq!(task["outcome"]["kind"], "cancelled", "{task}");
        assert_eq!(task["outcome"]["escalated_to"], "sigkill", "{task}");
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "escalation did not complete"
        );

        // Cancel is idempotent and answers the settled fact afterwards.
        let late = execute_call(
            &mut f.parent,
            &f.bound,
            "shell.task.cancel",
            operation_request(&f.bound, operation),
        )
        .await
        .expect("an idempotent cancel");
        assert_eq!(late["outcome"], "already_settled", "{late}");

        // SOURCE mode, which is where the interesting failure was: the
        // interpreter catches SIGTERM for its own graceful shutdown, so a
        // cancelled task could write a SUCCESSFUL frame and exit 0 — making a
        // killed evaluation indistinguishable from one that returned nil.
        let operation = submit_task(
            &mut f.parent,
            &f.bound,
            3,
            serde_json::json!({
                "source": "print(\"RUNNING\")\nsleep(30)\n1",
                "timeout_ms": "20000",
            }),
        )
        .await
        .expect("admitted");
        // Observe `cancelling` positively: between the request and the wait
        // status there is a real state, and a caller polling through it must
        // see something other than "running".
        let mut seen_cancelling = false;
        tokio::time::sleep(Duration::from_millis(700)).await;
        execute_call(
            &mut f.parent,
            &f.bound,
            "shell.task.cancel",
            operation_request(&f.bound, operation),
        )
        .await
        .expect("a live task resolves");
        for _ in 0..20 {
            let state = execute_call(
                &mut f.parent,
                &f.bound,
                "shell.task.result",
                operation_request(&f.bound, operation),
            )
            .await
            .expect("a known task always has a state");
            if state["state"] == "cancelling" {
                seen_cancelling = true;
            }
            if state["state"] == "settled" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let report = task_report(&mut f.parent, &f.bound, operation).await;
        let task = &report["report"];
        assert_eq!(task["outcome"]["kind"], "cancelled", "{task}");
        assert!(seen_cancelling, "the cancelling state was never observable");
        // The frame must NOT claim success. Either the interpreter got far
        // enough to write an interrupted-by-signal error, or it was killed
        // before writing at all — never {ok:true}.
        let result = &task["result"];
        if result["kind"] == "value" {
            let data = result["data"].as_str().unwrap_or_default();
            assert!(
                data.contains("\"ok\":false") || data.contains("\"ok\": false"),
                "a cancelled source task reported success: {data}"
            );
        } else {
            assert!(
                result["kind"] == "result_missing" || result["kind"] == "result_torn",
                "unexpected result for a cancelled source task: {result}"
            );
        }

        // DETERMINISTIC result_torn, through the test hook, because signals
        // cannot produce one: this interpreter handles SIGTERM gracefully and
        // writes a COMPLETE error frame, which is the previous case above. A
        // half-written frame only happens when a writer dies mid-write, so the
        // hook reproduces exactly that and nothing else.
        let operation = submit_task(
            &mut f.parent,
            &f.bound,
            4,
            serde_json::json!({
                "source": "41 + 1",
                "env": [["MIX_RESULT_TORN", "1"]],
                "timeout_ms": "20000",
            }),
        )
        .await
        .expect("admitted");
        let report = task_report(&mut f.parent, &f.bound, operation).await;
        let task = &report["report"];
        // The TASK succeeded — this is the point. A frame the supervisor could
        // not read must not be reported as the task having failed.
        assert_eq!(task["outcome"]["kind"], "exited", "{task}");
        assert_eq!(task["outcome"]["code"], 0, "{task}");
        assert_eq!(
            task["result"]["kind"], "result_torn",
            "a half-written frame must report result_torn: {task}"
        );
        teardown(f).await;
    });
}

/// Bounds and refusals: nothing is silent, nothing wedges, and the deferrals
/// are advertised rather than left to look like typos.
#[test]
fn p4_bounds_are_reported_and_refusals_leave_no_trace() {
    let _fixture = fixture_guard();
    runtime().block_on(async {
        let mut f = stage_d_fixture("owned").await;

        // Truncation is reported with the REAL byte count, and never turns a
        // successful task into a failed one.
        let operation = submit_task(
            &mut f.parent,
            &f.bound,
            1,
            serde_json::json!({
                "argv": ["sh", "-c", "yes ABCDEFGHIJ | head -c 300000"],
                "timeout_ms": "20000",
            }),
        )
        .await
        .expect("admitted");
        let report = task_report(&mut f.parent, &f.bound, operation).await;
        let task = &report["report"];
        assert_eq!(task["stdout"]["truncated"], true, "{task}");
        assert_eq!(
            counter(&task["stdout"]["bytes"]),
            300_000,
            "the REAL byte count must survive truncation: {task}"
        );
        assert!(task["stdout"]["text"].as_str().unwrap().len() <= 64 * 1024);
        assert_eq!(task["outcome"]["code"], 0, "{task}");

        // stderr has its own budget, and a NUL-heavy stream is the case the
        // raw-byte caps got wrong: one byte to capture, six to encode. The cap
        // that matters is the one the reply pays.
        let operation = submit_task(
            &mut f.parent,
            &f.bound,
            3,
            serde_json::json!({
                "argv": ["sh", "-c", "head -c 200000 /dev/zero >&2; echo done"],
                "timeout_ms": "20000",
            }),
        )
        .await
        .expect("admitted");
        let report = task_report(&mut f.parent, &f.bound, operation).await;
        let task = &report["report"];
        assert_eq!(task["stderr"]["truncated"], true, "{task}");
        assert_eq!(counter(&task["stderr"]["bytes"]), 200_000, "{task}");
        let encoded = serde_json::to_string(&task["stderr"]["text"]).expect("encodes");
        assert!(
            encoded.len() <= 64 * 1024,
            "stderr costs {} encoded bytes, over its budget",
            encoded.len()
        );
        // The whole settled reply has to fit one Term reply, which is the
        // property the per-field budgets exist to produce.
        assert!(
            serde_json::to_string(task).expect("encodes").len() < 256 * 1024,
            "the settled report does not fit a single reply"
        );

        // A result larger than the frame cap comes back as a truncated
        // REFERENCE — the caller learns a value existed and why it is absent,
        // rather than the frame being cut and read as a killed writer.
        let operation = submit_task(
            &mut f.parent,
            &f.bound,
            4,
            serde_json::json!({
                "source": "repeat(\"x\", 200000)",
                "timeout_ms": "20000",
            }),
        )
        .await
        .expect("admitted");
        let report = task_report(&mut f.parent, &f.bound, operation).await;
        let result = &report["report"]["result"];
        assert_eq!(result["kind"], "value", "{result}");
        let data = result["data"].as_str().unwrap();
        assert!(
            data.contains("truncated"),
            "an oversized value must report itself truncated: {data}"
        );
        assert_eq!(report["report"]["outcome"]["code"], 0, "{report}");

        // The quote-heavy case, which is where a raw-byte view and an encoded
        // view diverge most: a few thousand short strings produce a COMPLETE
        // frame — the writer's own cap is satisfied — whose JSON cost is far
        // over the reply budget. The settle path must hand back a reference,
        // never a data string cut mid-token, which would be unparseable and
        // would not say so.
        let operation = submit_task(
            &mut f.parent,
            &f.bound,
            6,
            serde_json::json!({
                "source": "$out = []\nfor $i in range(0, 7000)\n  push($out, \"s\" + $i)\nend\n$out\n",
                "timeout_ms": "20000",
            }),
        )
        .await
        .expect("admitted");
        let report = task_report(&mut f.parent, &f.bound, operation).await;
        let result = &report["report"]["result"];
        assert_eq!(result["kind"], "value", "{result}");
        let data = result["data"].as_str().unwrap();
        assert_eq!(
            report["report"]["outcome"]["code"], 0,
            "the task itself succeeded: {report}"
        );
        // Whatever comes back must PARSE. That is the property a cut string
        // breaks, and asserting it directly is what makes this fixture able to
        // fail rather than merely observe.
        let parsed: serde_json::Value = serde_json::from_str(data)
            .unwrap_or_else(|e| panic!("the result data must be parseable ({e}): {data}"));
        assert_eq!(parsed["truncated"], true, "{data}");
        assert!(parsed["bytes"].is_string(), "{data}");

        // A cwd that does not exist is refused BEFORE any spawn, and the
        // request id is untouched — so the same id may simply be retried.
        let error = submit_task(
            &mut f.parent,
            &f.bound,
            7,
            serde_json::json!({"source": "1", "cwd": "/nonexistent/p4"}),
        )
        .await
        .unwrap_err();
        assert!(error.contains("NOT_FOUND"), "{error}");
        let operation = submit_task(&mut f.parent, &f.bound, 7, serde_json::json!({"source": "1"}))
            .await
            .expect("a refused task must not burn its request id");
        task_report(&mut f.parent, &f.bound, operation).await;

        // A zero timeout is refused, and so is one past the cap.
        for timeout in ["0", "600001"] {
            let error = submit_task(
                &mut f.parent,
                &f.bound,
                50,
                serde_json::json!({"source": "1", "timeout_ms": timeout}),
            )
            .await
            .unwrap_err();
            assert!(error.contains("INVALID_ARGUMENT"), "{timeout}: {error}");
        }

        // Concurrency is a REAL limit — processes and supervisor threads — so
        // it refuses where the record table evicts. Short, self-terminating
        // sleepers: the cap is what is under test, not the wait.
        let mut held = Vec::new();
        for id in 10..14u64 {
            held.push(
                submit_task(
                    &mut f.parent,
                    &f.bound,
                    id,
                    serde_json::json!({"argv": ["sleep", "3"], "timeout_ms": "4000"}),
                )
                .await
                .unwrap_or_else(|e| panic!("task {id} should start: {e}")),
            );
        }
        let error = submit_task(
            &mut f.parent,
            &f.bound,
            14,
            serde_json::json!({"argv": ["sleep", "3"], "timeout_ms": "4000"}),
        )
        .await
        .unwrap_err();
        assert!(error.contains("RESOURCE_LIMIT"), "{error}");
        for operation in held {
            let _ = execute_call(
                &mut f.parent,
                &f.bound,
                "shell.task.cancel",
                operation_request(&f.bound, operation),
            )
            .await;
        }

        // The deferrals answer UNSUPPORTED explicitly, and — the part that
        // makes this fixture mean anything — DIFFERENTLY from a verb that does
        // not exist. Asserting only the code passed with the deferral arms
        // deleted, because an unknown verb is refused the same way.
        for verb in ["shell.task.watch", "shell.task.list"] {
            let error = execute_call(
                &mut f.parent,
                &f.bound,
                verb,
                operation_request(&f.bound, 1),
            )
            .await
            .unwrap_err();
            assert!(error.contains("UNSUPPORTED"), "{verb}: {error}");
            assert!(
                error.contains("deferred"),
                "{verb} is a deferral and must say so: {error}"
            );
        }
        let typo = execute_call(
            &mut f.parent,
            &f.bound,
            "shell.task.wtach",
            operation_request(&f.bound, 1),
        )
        .await
        .unwrap_err();
        assert!(typo.contains("UNSUPPORTED"), "{typo}");
        assert!(
            !typo.contains("deferred"),
            "an unknown verb must not look like a deferral: {typo}"
        );
        teardown(f).await;
    });
}

/// What a task LEAVES BEHIND must not be able to hold the shell hostage.
#[test]
fn p4_a_survivor_cannot_wedge_the_supervisor() {
    let _fixture = fixture_guard();
    runtime().block_on(async {
        let mut f = stage_d_fixture("owned").await;

        // The wedge, exactly: `sh` backgrounds a long sleeper and exits. The
        // sleeper inherited stdout, stderr and — in source mode — the result
        // descriptor, so reading any of them to EOF waits for the SLEEPER, not
        // the task. Unbounded, this pinned the record at "running" for ten
        // minutes and never gave back its TASKS slot.
        let started = Instant::now();
        let mut survivors = Vec::new();
        for id in 1..=4u64 {
            survivors.push(
                submit_task(
                    &mut f.parent,
                    &f.bound,
                    id,
                    serde_json::json!({
                        "argv": ["sh", "-c", "sleep 600 & echo SPAWNED"],
                        "timeout_ms": "10000",
                    }),
                )
                .await
                .unwrap_or_else(|e| panic!("task {id} should start: {e}")),
            );
        }
        for operation in &survivors {
            let report = task_report(&mut f.parent, &f.bound, *operation).await;
            let task = &report["report"];
            // The task itself exited cleanly and promptly; that is the answer,
            // and it does not wait on what the task left running.
            assert_eq!(task["outcome"]["kind"], "exited", "{task}");
            assert_eq!(task["outcome"]["code"], 0, "{task}");
            assert!(
                task["stdout"]["text"].as_str().unwrap().contains("SPAWNED"),
                "{task}"
            );
            // And the report is HONEST that it stopped listening rather than
            // presenting a bounded read as the whole of the output.
            assert_eq!(
                task["stdout"]["writer_survived"], true,
                "a held-open pipe must be reported, not hidden: {task}"
            );
        }
        // And the survivors are DEAD, not merely stopped waiting for. The
        // settlement kill reaches the whole group while the un-reaped leader
        // still pins it, which is the only thing that can catch a child the
        // task backgrounded: pdeathsig cannot see it, and by shell exit it
        // would long since have been orphaned.
        let leftovers = std::process::Command::new("pgrep")
            .args(["-f", "sleep 600"])
            .output()
            .expect("pgrep runs");
        assert!(
            String::from_utf8_lossy(&leftovers.stdout).trim().is_empty(),
            "a settled task left its backgrounded children alive: {}",
            String::from_utf8_lossy(&leftovers.stdout)
        );
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "four survivors took {:?} — the drains are not bounded",
            started.elapsed()
        );

        // The TASKS slots came BACK. Four wedged supervisors used to mean the
        // shell refused every later task for the rest of its life.
        let operation = submit_task(
            &mut f.parent,
            &f.bound,
            9,
            serde_json::json!({"source": "40 + 2"}),
        )
        .await
        .expect("the concurrency slots must be released by a settled task");
        let report = task_report(&mut f.parent, &f.bound, operation).await;
        assert!(
            report["report"]["result"]["data"]
                .as_str()
                .unwrap_or_default()
                .contains("42"),
            "{report}"
        );

        // Kill-on-drop: the shell leaves, and what its tasks left running goes
        // with it. PDEATHSIG reaches the leader; only the sweep reaches the
        // group, which is where a backgrounded grandchild lives.
        let operation = submit_task(
            &mut f.parent,
            &f.bound,
            10,
            serde_json::json!({
                "argv": ["sh", "-c", "sleep 913 & echo $! ; sleep 913"],
                "timeout_ms": "60000",
            }),
        )
        .await
        .expect("admitted");
        let _ = operation;
        tokio::time::sleep(Duration::from_millis(600)).await;
        let shell = f.child.pid();
        let group_alive = |pid: i32| {
            std::path::Path::new(&format!("/proc/{pid}")).exists()
        };
        assert!(group_alive(shell), "the shell should still be up");
        f.child.exit();
        let gone = Instant::now() + Duration::from_secs(15);
        while group_alive(shell) && Instant::now() < gone {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // The sleepers are in the task's process group, which the sweep
        // SIGKILLs on the way out; nothing is left for `ps` to find.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let leftovers = std::process::Command::new("pgrep")
            .args(["-f", "sleep 913"])
            .output()
            .expect("pgrep runs");
        assert!(
            String::from_utf8_lossy(&leftovers.stdout).trim().is_empty(),
            "a task's children outlived the shell: {}",
            String::from_utf8_lossy(&leftovers.stdout)
        );
    });
}
