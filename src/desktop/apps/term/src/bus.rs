use crate::terminal::Terminal;
use cosmix_client::{BoundedIncomingEvent, SupervisedClient};
use std::{
    sync::{Arc, Mutex, atomic::Ordering},
    time::Duration,
};

pub const HELP: &str = "term: one-pane Wayland Mix terminal\nDIAGNOSTIC surface — full ABP control (windows/tabs/panes/sessions per SPEC) is P3a, gated on authenticated per-instance identity (P0-I); this self-asserted `term` name is a placeholder, not the shipped multi-user identity.\nINFO / HELP\nterm.snapshot: read-only screen, dimensions, cursor, child pid, byte counters and DIAGNOSTIC timings\nterm.type: DIAGNOSTIC ONLY; ASCII synthetic keys through the keyboard encoder, max 8192 bytes; newline=Enter, tab, backspace, Ctrl+C/D supported. Not a product input API.\nDIAGNOSTIC timings are process-side, never presented-frame evidence.";
pub fn start(terminal: Arc<Mutex<Terminal>>) {
    let listener = terminal.lock().unwrap().listener.clone();
    let stats = terminal.lock().unwrap().stats.clone();
    let diagnostic_quit = listener.quit.clone();
    std::thread::spawn(move || {
        let mut last = std::time::Instant::now();
        while !diagnostic_quit.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(100));
            if last.elapsed() >= Duration::from_secs(10) {
                println!("{}", stats.lock().unwrap().summary());
                last = std::time::Instant::now();
            }
        }
    });
    std::thread::Builder::new().name("term-bus".into()).spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("Bus runtime");
        runtime.block_on(async move {
            let result = SupervisedClient::connect_options("term", &cosmix_config::client_helpers::resolve_noded_url()).bounded_incoming(16).connect().await;
            let client = match result { Ok(client) => client, Err(e) => { eprintln!("term Bus unavailable: {e}"); return; } };
            let Some(mut incoming) = client.incoming_bounded() else { return; };
            while !listener.quit.load(Ordering::Acquire) {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {},
                    event = incoming.recv() => {
                        let command = match event {
                            Some(BoundedIncomingEvent::Command(c)) => c,
                            Some(BoundedIncomingEvent::Overflow { .. }) => { eprintln!("term Bus incoming overflow"); continue; },
                            None => break,
                        };
                        let result = if command.body.len() > 8192 { Err("request exceeds 8192 bytes".into()) } else {
                            match command.command.as_str() {
                                "INFO" | "HELP" | "info" | "help" => Ok(HELP.into()),
                                "term.snapshot" => Ok(terminal.lock().unwrap().snapshot()),
                                "term.type" => listener.type_text(&command.body).map(|_| "DIAGNOSTIC synthetic keys queued; inspect input_written for actual writes".into()),
                                _ => Err("unknown verb; use HELP".into()),
                            }
                        };
                        let (rc,body) = match result { Ok(body) => (0,body), Err(error) => (10,error) };
                        let _ = tokio::time::timeout(Duration::from_secs(2),client.respond(&command,rc,&body)).await;
                    }
                }
            }
            client.close().await;
        });
    }).expect("Bus thread");
}
