//! p0i-01: production Term prepare/fd mapping/Machine path to a real Mix proof.
//! See tests/README.md for the explicit current-HEAD release binary prerequisite.
use super::*;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use term_native_test_broker::Broker;

fn current_mix() -> Option<PathBuf> {
    let Some(path) = std::env::var_os("COSMIX_E2E_MIX_BIN") else {
        // Direct stderr is deliberate: libtest must not hide the skip in its
        // successful-test capture. An absent prerequisite is not E2E evidence.
        writeln!(std::io::stderr(), "SKIPPED p0i-01 production Term→Mix proof: build current HEAD with cargo build --release -p cosmix-mix, then set COSMIX_E2E_MIX_BIN to target/release/mix").unwrap();
        return None;
    };
    let binary = PathBuf::from(path)
        .canonicalize()
        .expect("COSMIX_E2E_MIX_BIN must name an existing current-HEAD Mix binary");
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let status = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=normal"])
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        status.status.success() && status.stdout.is_empty(),
        "p0i-01 requires a clean current checkout, including dependency edits"
    );
    let head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(head.status.success());
    let head = String::from_utf8(head.stdout).unwrap();
    let version = Command::new(&binary)
        .args(["--version", "--json"])
        .env_remove(crate::session_fd::MARKER)
        .env("MIX_STATS", "off")
        .output()
        .unwrap();
    assert!(version.status.success(), "Mix provenance probe failed");
    let version: serde_json::Value = serde_json::from_slice(&version.stdout).unwrap();
    assert_eq!(
        version["git_sha_full"].as_str(),
        Some(head.trim()),
        "STALE p0i-01 Mix: rebuild current HEAD; no installed-binary fallback"
    );
    assert_eq!(
        version["git_dirty"], false,
        "rebuild Mix from a clean checkout"
    );
    eprintln!(
        "p0i-01 production binary={} commit={}",
        binary.display(),
        head.trim()
    );
    Some(binary)
}

fn account_name() -> String {
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
    unsafe { std::ffi::CStr::from_ptr(entry.pw_name) }
        .to_str()
        .unwrap()
        .to_owned()
}

#[test]
fn p0i_01_production_term_spawn_enrols_real_mix_and_exit_revokes() {
    let Some(binary) = current_mix() else {
        return;
    };
    let broker = Broker::start();
    let root = broker.endpoint.parent().unwrap();
    let home = root.join("mix-home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join(".mixrc"),
        "fn prompt()\nreturn \"P0I01> \"\nend\n",
    )
    .unwrap();
    let config = root.join("mix-node.conf.mix");
    std::fs::write(
        &config,
        format!(
            "noded: {{ unix_socket: {} }}\n",
            serde_json::to_string(&broker.endpoint).unwrap()
        ),
    )
    .unwrap();
    let supervisor = Supervisor::with_options(broker.options(), broker.url.clone()).unwrap();
    super::tests::wait_ready(&supervisor.handle, 1);
    // No parent environment mutation, no substitute PTY launcher. Only the
    // executable and isolated launch configuration differ from normal Term.
    let terminal = crate::terminal::Terminal::start_session_e2e(
        crate::config::Settings {
            config: crate::config::Config::default(),
            term: "xterm-256color",
        },
        &supervisor.handle,
        binary.to_str().unwrap(),
        home.display().to_string(),
        vec![
            ("HOME".into(), home.display().to_string()),
            ("COSMIX_SRC".into(), home.display().to_string()),
            ("COSMIX_NODE_CONFIG".into(), config.display().to_string()),
            ("COSMIX_BROKER_ACCOUNT".into(), account_name()),
            ("MIX_STATS".into(), "off".into()),
            ("MIX_EDITOR".into(), "owned".into()),
        ],
    )
    .expect("production Term spawn");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let UnixConnectOutcome::VerifiedUnix(observer) =
            NodedClient::connect_unix("", &broker.url, &broker.options(), None)
                .await
                .unwrap()
        else {
            panic!("verified observer required")
        };
        let bound = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Some(record) = observer
                    .session_list()
                    .await
                    .unwrap()
                    .records
                    .into_iter()
                    .find(|r| r.pane_id == Some(DecimalU64(1)) && r.state == BindingState::Attached)
                {
                    break record;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "production Mix did not enrol: {}\n{}",
                supervisor.handle.status(),
                terminal.snapshot()
            )
        });
        assert_eq!(bound.binding_generation, DecimalU64(1));
        assert_eq!(bound.role, Role::PaneShell);
        tokio::time::timeout(Duration::from_secs(10), async {
            while !terminal.snapshot().contains("P0I01>") {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("real Mix prompt");
        terminal.listener.type_text("exit\n").unwrap();
        // Keep Terminal alive: only the real PTY child-event notifier can
        // drive this revoke. Dropping Terminal first would make it vacuous.
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let record = observer.session_self(bound.record_id).await.unwrap().record;
                if record.state == BindingState::Revoked
                    && terminal.listener.quit.load(Ordering::Acquire)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("actual Mix exit must revoke through MeteredPty");
    });
    drop(terminal);
}
