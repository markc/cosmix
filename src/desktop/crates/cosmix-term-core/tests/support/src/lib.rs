//! Tests-only embedding of the real noded, with no broker source refactor.
//! Keep module paths pointed at production sources: no simulated session RPCs.
// This crate has no tests of its own ([lib] test = false), but
// `cargo build --all-targets` still builds its lib-test target. Under cfg(test)
// the embedded noded and session_fd sources compile their own test modules,
// which need noded's dev-dependencies. Build that target as an empty crate.
#![cfg(not(test))]
#![allow(dead_code)]

// Compile the real handoff as library code, without pulling Term's desktop
// teletypewriter tests into the main workspace's Mix integration target.
#[path = "../../../src/session_fd.rs"]
pub mod session_fd;

#[path = "../../../../../../crates/cosmix-noded/src/admission.rs"]
mod admission;
#[path = "../../../../../../crates/cosmix-noded/src/authority.rs"]
mod authority;
#[path = "../../../../../../crates/cosmix-noded/src/native_ingress.rs"]
mod native_ingress;
#[path = "../../../../../../crates/cosmix-noded/src/noded.rs"]
mod noded;
#[path = "../../../../../../crates/cosmix-noded/src/observe.rs"]
mod observe;
#[path = "../../../../../../crates/cosmix-noded/src/props.rs"]
mod props;
#[path = "../../../../../../crates/cosmix-noded/src/props_reservation.rs"]
mod props_reservation;
#[path = "../../../../../../crates/cosmix-noded/src/protection.rs"]
mod protection;
#[path = "../../../../../../crates/cosmix-noded/src/routing.rs"]
mod routing;
#[path = "../../../../../../crates/cosmix-noded/src/spec.rs"]
mod spec;
#[path = "../../../../../../crates/cosmix-noded/src/spec_release.rs"]
mod spec_release;
#[path = "../../../../../../crates/cosmix-noded/src/subscription.rs"]
mod subscription;

use std::path::PathBuf;
use std::time::Duration;

/// Used only in an isolated acceptance subprocess so actual broker diagnostic
/// logging is included in the parent's stdout/stderr confidentiality check.
pub fn trace_to_stderr() {
    use tracing_subscriber::prelude::*;
    let targets = tracing_subscriber::filter::Targets::new()
        .with_target("term_native_test_broker", tracing::Level::TRACE)
        .with_target("cosmix_client", tracing::Level::TRACE)
        .with_target("term", tracing::Level::TRACE);
    tracing_subscriber::registry().with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr).with_filter(targets)).try_init().unwrap();
}

pub struct Broker {
    pub endpoint: PathBuf,
    pub url: String,
    root: PathBuf,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
    pause: Option<tokio::sync::mpsc::UnboundedSender<PauseRequest>>,
    grant_limit: usize,
}

type PauseRequest = (
    std::sync::mpsc::SyncSender<()>,
    std::sync::mpsc::Receiver<()>,
);
pub struct Paused(std::sync::mpsc::SyncSender<()>);
impl Drop for Paused {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

impl Default for Broker {
    fn default() -> Self {
        Self::start()
    }
}

impl Broker {
    pub fn start() -> Self {
        Self::with_grant_limit(32)
    }

    pub fn with_grant_limit(grant_limit: usize) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let root =
            std::env::temp_dir().join(format!("term-native-{:032x}", rand::random::<u128>()));
        std::fs::create_dir(&root).unwrap();
        // BUS-013 requires user-traversable, broker-owned ancestors. A 0700
        // directory deliberately disables noded's native profile; TCP readiness
        // alone cannot establish that a native-session fixture is usable.
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut broker = Self {
            endpoint: root.join("bus.sock"),
            root,
            url: String::new(),
            stop: None,
            worker: None,
            pause: None,
            grant_limit,
        };
        broker.boot();
        broker
    }

    pub fn options(&self) -> cosmix_client::UnixConnectOptions {
        let mut options = cosmix_client::UnixConnectOptions::new(cosmix_client::BrokerAccount {
            // Real kernel credentials of the embedded broker, not a wire claim.
            uid: unsafe { libc::geteuid() },
            gid: unsafe { libc::getegid() },
        });
        options.endpoint = Some(self.endpoint.clone());
        options.require_native_session = true;
        options
    }

    fn boot(&mut self) {
        // Bound here and handed to noded as-is. Probing `:0`, dropping the
        // socket and passing only the number let another socket (a parallel
        // test's outbound connection, say) take the port first; noded's bind
        // then failed, `run` returned before readiness, and the whole test
        // binary aborted on the fallout — 3 first runs in 8 on cbc2/cbc3.
        let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        tcp.set_nonblocking(true).unwrap();
        let listen = tcp.local_addr().unwrap().to_string();
        self.url = format!("ws://{listen}/ws");
        let endpoint = self.endpoint.clone();
        let options = self.options();
        let url = self.url.clone();
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();
        let (pause_tx, mut pause_rx) = tokio::sync::mpsc::unbounded_channel::<PauseRequest>();
        self.pause = Some(pause_tx);
        let grant_limit = self.grant_limit;
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        self.stop = Some(stop_tx);
        self.worker = Some(std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let (tx, rx) = tokio::sync::oneshot::channel();
                let tcp = tokio::net::TcpListener::from_std(tcp).unwrap();
                let broker = tokio::spawn(noded::run_on(
                    noded::RunConfig {
                        unix_socket: Some(endpoint),
                        pending_grants_per_parent: grant_limit,
                        listen,
                        node: "test-node".into(),
                        wg_ip: "127.0.0.1".into(),
                        mesh_config_path: None,
                        mesh_open: false,
                        spec_dir: None,
                        admission_mode: cosmix_config::node::AdmissionMode::Off,
                        observe_allowed_services: vec!["term-policy-audit".into()],
                    },
                    tcp,
                    tx,
                ));
                match tokio::time::timeout(Duration::from_secs(5), rx).await {
                    Ok(Ok(())) => {}
                    // run() returned before readiness: say why, instead of
                    // leaving boot() to report only a closed channel.
                    Ok(Err(_)) => {
                        let reason = match broker.await {
                            Ok(Err(error)) => format!("{error:#}"),
                            Ok(Ok(())) => "returned Ok before readiness".into(),
                            Err(error) => format!("task failed: {error}"),
                        };
                        let _ = ready_tx.send(Err(format!("embedded noded exited: {reason}")));
                        return;
                    }
                    Err(_) => {
                        broker.abort();
                        let _ = ready_tx.send(Err("embedded noded not ready within 5s".into()));
                        return;
                    }
                }
                let probe = tokio::time::timeout(
                    Duration::from_secs(5),
                    cosmix_client::NodedClient::connect_unix("", &url, &options, None),
                )
                .await
                .expect("native fixture profile negotiation deadline")
                .expect("native fixture must provide verified Unix ingress");
                let cosmix_client::UnixConnectOutcome::VerifiedUnix(probe) = probe else {
                    panic!("native fixture must not fall back to TCP");
                };
                probe.client().close().await;
                let _ = ready_tx.send(Ok(()));
                loop {
                    tokio::select! {
                        _ = &mut stop_rx => break,
                        Some((entered, release)) = pause_rx.recv() => {
                            // Deliberately stall the real single-thread broker
                            // runtime, including accepted UDS sockets. Drop of
                            // Paused releases it even if a test assertion panics.
                            let _ = entered.send(());
                            let _ = release.recv_timeout(Duration::from_secs(15));
                        }
                    }
                }
                broker.abort();
                let _ = broker.await;
            });
            // Dropping the whole runtime also closes accepted sockets, timers
            // and spawned routing tasks. Aborting only run() is not a bounce.
        }));
        match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(())) => {}
            Ok(Err(reason)) => panic!("test broker failed to start: {reason}"),
            Err(error) => panic!("test broker failed to start: worker gave no readiness ({error})"),
        }
    }

    /// Never panics: `Drop` calls it, and a destructor panic while a failing
    /// test is already unwinding aborts the whole test binary. A worker panic
    /// has printed its own message by the time it is joined here.
    pub fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            eprintln!("test broker worker panicked; its message is above");
        }
    }

    pub fn pause(&self) -> Paused {
        let (entered, ack) = std::sync::mpsc::sync_channel(1);
        let (release, resumed) = std::sync::mpsc::sync_channel(1);
        self.pause
            .as_ref()
            .unwrap()
            .send((entered, resumed))
            .unwrap();
        ack.recv_timeout(Duration::from_secs(3)).unwrap();
        Paused(release)
    }

    pub fn bounce(&mut self) {
        self.stop();
        self.boot();
    }
}

impl Drop for Broker {
    fn drop(&mut self) {
        self.stop();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
