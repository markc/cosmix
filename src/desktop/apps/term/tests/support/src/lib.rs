//! Tests-only embedding of the real noded, with no broker source refactor.
//! Keep module paths pointed at production sources: no simulated session RPCs.
#![allow(dead_code)]

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

pub struct Broker {
    pub endpoint: PathBuf,
    pub url: String,
    root: PathBuf,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Default for Broker {
    fn default() -> Self {
        Self::start()
    }
}

impl Broker {
    pub fn start() -> Self {
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
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let listen = probe.local_addr().unwrap().to_string();
        drop(probe);
        self.url = format!("ws://{listen}/ws");
        let endpoint = self.endpoint.clone();
        let options = self.options();
        let url = self.url.clone();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        self.stop = Some(stop_tx);
        self.worker = Some(std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let (tx, rx) = tokio::sync::oneshot::channel();
                let broker = tokio::spawn(noded::run(
                    noded::RunConfig {
                        unix_socket: Some(endpoint),
                        pending_grants_per_parent: 32,
                        listen,
                        node: "test-node".into(),
                        wg_ip: "127.0.0.1".into(),
                        mesh_config_path: None,
                        spec_dir: None,
                        admission_mode: cosmix_config::node::AdmissionMode::Off,
                        observe_allowed_services: Vec::new(),
                    },
                    tx,
                ));
                tokio::time::timeout(Duration::from_secs(5), rx)
                    .await
                    .unwrap()
                    .unwrap();
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
                ready_tx.send(()).unwrap();
                let _ = stop_rx.await;
                broker.abort();
                let _ = broker.await;
            });
            // Dropping the whole runtime also closes accepted sockets, timers
            // and spawned routing tasks. Aborting only run() is not a bounce.
        }));
        ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    }

    pub fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
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
