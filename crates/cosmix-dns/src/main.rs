//! cosmix-dns — Authoritative DNS server with zone management UI.
//!
//! Provides a desktop interface for:
//! - Viewing and editing zone files
//! - Browsing records within zones
//! - Adding/removing records
//! - Starting/stopping the DNS server process
//!
//! Zone files are stored in a configurable directory (default: /var/lib/hickory/).

use std::path::PathBuf;

use dioxus::prelude::*;
use hickory_proto::rr::Name;
use hickory_proto::serialize::txt::Parser;

fn main() {
    cosmix_ui::desktop::init_linux_env();

    #[cfg(feature = "desktop")]
    {
        let cfg = cosmix_ui::desktop::window_config("cosmix-dns", 960.0, 640.0);
        LaunchBuilder::new().with_cfg(cfg).launch(app);
        return;
    }

    #[allow(unreachable_code)]
    {
        eprintln!("Desktop feature not enabled");
        std::process::exit(1);
    }
}

// ── Zone data ──

fn zone_dir() -> PathBuf {
    PathBuf::from(
        std::env::var("HICKORY_ZONE_DIR").unwrap_or_else(|_| "/var/lib/hickory".to_string()),
    )
}

#[derive(Clone, Debug)]
struct ZoneInfo {
    filename: String,
    origin: String,
    record_count: usize,
}

#[derive(Clone, Debug)]
struct RecordEntry {
    name: String,
    rtype: String,
    ttl: u32,
    rdata: String,
}

fn discover_zones() -> Vec<ZoneInfo> {
    let dir = zone_dir();
    let mut zones = Vec::new();

    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("cannot read zone dir {}: {e}", dir.display());
            return zones;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let filename = entry.file_name().to_string_lossy().to_string();

        if !filename.ends_with(".zone") {
            continue;
        }

        match std::fs::read_to_string(&path) {
            Ok(content) => {
                // Derive origin from filename: example.com.zone -> example.com.
                let origin_str = filename.trim_end_matches(".zone").to_string();
                let origin_name = Name::from_ascii(&origin_str).ok();

                let parser = Parser::new(&content, Some(path.clone()), origin_name.clone());
                match parser.parse() {
                    Ok((origin, records)) => {
                        let count: usize = records.values().map(|rs| rs.records_without_rrsigs().count()).sum();
                        zones.push(ZoneInfo {
                            filename,
                            origin: origin.to_string(),
                            record_count: count,
                        });
                    }
                    Err(e) => {
                        tracing::warn!("failed to parse {}: {e}", path.display());
                        zones.push(ZoneInfo {
                            filename,
                            origin: origin_str,
                            record_count: 0,
                        });
                    }
                }
            }
            Err(e) => {
                tracing::warn!("cannot read {}: {e}", path.display());
            }
        }
    }

    zones.sort_by(|a, b| a.origin.cmp(&b.origin));
    zones
}

fn load_zone_records(filename: &str) -> Result<(String, Vec<RecordEntry>), String> {
    let path = zone_dir().join(filename);
    let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;

    let origin_str = filename.trim_end_matches(".zone");
    let origin_name = Name::from_ascii(origin_str).ok();

    let parser = Parser::new(&content, Some(path), origin_name);
    let (origin, records) = parser.parse().map_err(|e| e.to_string())?;

    let mut entries = Vec::new();
    for (_key, rrset) in &records {
        let name = rrset.name().to_string();
        let rtype = rrset.record_type().to_string();
        let ttl = rrset.ttl();

        for record in rrset.records_without_rrsigs() {
            let rdata = record
                .data()
                .to_string();
            entries.push(RecordEntry {
                name: name.clone(),
                rtype: rtype.clone(),
                ttl,
                rdata,
            });
        }
    }

    // Sort: SOA first, then NS, then alphabetical by name+type
    entries.sort_by(|a, b| {
        let type_order = |t: &str| match t {
            "SOA" => 0,
            "NS" => 1,
            "MX" => 2,
            "A" => 3,
            "AAAA" => 4,
            "CNAME" => 5,
            "TXT" => 6,
            "SRV" => 7,
            _ => 8,
        };
        type_order(&a.rtype)
            .cmp(&type_order(&b.rtype))
            .then(a.name.cmp(&b.name))
    });

    Ok((origin.to_string(), entries))
}

fn read_zone_file_raw(filename: &str) -> Result<String, String> {
    let path = zone_dir().join(filename);
    std::fs::read_to_string(&path).map_err(|e| e.to_string())
}

fn write_zone_file_raw(filename: &str, content: &str) -> Result<(), String> {
    let path = zone_dir().join(filename);
    std::fs::write(&path, content).map_err(|e| e.to_string())
}

/// Check if hickory-dns server process is running
fn server_status() -> (bool, Option<u32>) {
    match std::process::Command::new("pgrep")
        .args(["-x", "hickory-dns"])
        .output()
    {
        Ok(output) => {
            if output.status.success() {
                let pid = String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .lines()
                    .next()
                    .and_then(|s| s.parse::<u32>().ok());
                (true, pid)
            } else {
                (false, None)
            }
        }
        Err(_) => (false, None),
    }
}

// ── UI ──

#[derive(Clone, PartialEq)]
enum View {
    ZoneList,
    ZoneRecords(String), // filename
    ZoneEdit(String),    // filename — raw text editor
}

fn app() -> Element {
    let mut zones: Signal<Vec<ZoneInfo>> = use_signal(Vec::new);
    let mut records: Signal<Vec<RecordEntry>> = use_signal(Vec::new);
    let mut zone_origin: Signal<String> = use_signal(String::new);
    let mut view: Signal<View> = use_signal(|| View::ZoneList);
    let mut error_msg: Signal<Option<String>> = use_signal(|| None);
    let mut edit_content: Signal<String> = use_signal(String::new);
    let mut running: Signal<bool> = use_signal(|| false);
    let mut pid: Signal<Option<u32>> = use_signal(|| None);

    // Load zones + check server status on startup
    use_effect(move || {
        zones.set(discover_zones());
        let (is_running, server_pid) = server_status();
        running.set(is_running);
        pid.set(server_pid);

        // Periodic server status check
        spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                let (is_running, server_pid) = server_status();
                running.set(is_running);
                pid.set(server_pid);
            }
        });
    });

    let mut open_zone = move |filename: String| {
        match load_zone_records(&filename) {
            Ok((origin, recs)) => {
                zone_origin.set(origin);
                records.set(recs);
                view.set(View::ZoneRecords(filename));
                error_msg.set(None);
            }
            Err(e) => error_msg.set(Some(e)),
        }
    };

    let mut open_editor = move |filename: String| {
        match read_zone_file_raw(&filename) {
            Ok(content) => {
                edit_content.set(content);
                view.set(View::ZoneEdit(filename));
                error_msg.set(None);
            }
            Err(e) => error_msg.set(Some(e)),
        }
    };

    let mut save_zone = move |filename: String| {
        let content = edit_content();
        match write_zone_file_raw(&filename, &content) {
            Ok(_) => {
                error_msg.set(None);
                // Re-parse to validate
                match load_zone_records(&filename) {
                    Ok((origin, recs)) => {
                        zone_origin.set(origin);
                        records.set(recs);
                        view.set(View::ZoneRecords(filename));
                        zones.set(discover_zones());
                    }
                    Err(e) => error_msg.set(Some(format!("Saved but parse error: {e}"))),
                }
            }
            Err(e) => error_msg.set(Some(e)),
        }
    };

    let back_to_list = move |_| {
        view.set(View::ZoneList);
        zones.set(discover_zones());
    };

    let zdir = zone_dir().display().to_string();

    rsx! {
        document::Style { "{CSS}" }
        div {
            style: "width:100%; height:100vh; display:flex; flex-direction:column; background:{BG_BASE}; color:{TEXT_PRIMARY}; font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Helvetica,Arial,sans-serif; font-size:13px;",

            // Header
            div {
                style: "padding:12px 16px; background:{BG_SURFACE}; border-bottom:1px solid {BORDER}; display:flex; align-items:center; gap:12px;",
                match &*view.read() {
                    View::ZoneList => rsx! {
                        span { style: "font-weight:600; font-size:15px;", "DNS Zones" }
                        span { style: "color:{TEXT_DIM}; font-size:12px;", "{zdir}" }
                    },
                    View::ZoneRecords(_) => rsx! {
                        button {
                            style: "background:{BG_ELEVATED}; border:1px solid {BORDER}; color:{TEXT_MUTED}; padding:4px 10px; border-radius:4px; cursor:pointer; font-size:12px;",
                            onclick: back_to_list,
                            "< Zones"
                        }
                        span { style: "font-weight:600; font-size:15px;", "{zone_origin}" }
                    },
                    View::ZoneEdit(_) => rsx! {
                        span { style: "font-weight:600; font-size:15px;", "Edit: {zone_origin}" }
                    },
                }

                // Server status + controls
                div { style: "margin-left:auto; display:flex; align-items:center; gap:8px;",
                    {
                        let status_color = if running() { "#22c55e" } else { "#ef4444" };
                        let status_text = if running() {
                            format!("running (pid {})", pid().unwrap_or(0))
                        } else {
                            "stopped".to_string()
                        };
                        rsx! {
                            span { style: "color:{status_color}; font-size:12px; font-weight:500;",
                                "{status_text}"
                            }
                        }
                    }

                    match &*view.read() {
                        View::ZoneRecords(f) => {
                            let f = f.clone();
                            rsx! {
                                button {
                                    style: "background:{BG_ELEVATED}; border:1px solid {BORDER}; color:{TEXT_MUTED}; padding:4px 10px; border-radius:4px; cursor:pointer; font-size:12px;",
                                    onclick: move |_| open_editor(f.clone()),
                                    "Edit"
                                }
                            }
                        }
                        View::ZoneEdit(f) => {
                            let f = f.clone();
                            let f2 = f.clone();
                            rsx! {
                                button {
                                    style: "background:{BG_ELEVATED}; border:1px solid {BORDER}; color:{TEXT_MUTED}; padding:4px 10px; border-radius:4px; cursor:pointer; font-size:12px;",
                                    onclick: move |_| save_zone(f.clone()),
                                    "Save"
                                }
                                button {
                                    style: "background:{BG_ELEVATED}; border:1px solid {BORDER}; color:{TEXT_MUTED}; padding:4px 10px; border-radius:4px; cursor:pointer; font-size:12px;",
                                    onclick: move |_| open_zone(f2.clone()),
                                    "Cancel"
                                }
                            }
                        }
                        _ => rsx! {}
                    }
                }
            }

            // Error banner
            if let Some(ref err) = error_msg() {
                div {
                    style: "padding:8px 16px; background:#7f1d1d; color:#fca5a5; font-size:12px;",
                    "{err}"
                }
            }

            // Content
            div { style: "flex:1; overflow-y:auto; padding:16px;",
                match &*view.read() {
                    View::ZoneList => rsx! {
                        div { style: "display:flex; flex-direction:column; gap:4px;",
                            for zone in zones().iter() {
                                {
                                    let filename = zone.filename.clone();
                                    rsx! {
                                        div {
                                            style: "display:flex; align-items:center; padding:10px 12px; background:{BG_SURFACE}; border-radius:6px; cursor:pointer; gap:12px;",
                                            onclick: move |_| open_zone(filename.clone()),
                                            span { style: "font-weight:500; flex:1;", "{zone.origin}" }
                                            span { style: "color:{TEXT_DIM}; font-size:11px;",
                                                "{zone.record_count} records"
                                            }
                                            span { style: "color:{TEXT_DIM}; font-size:11px; font-family:monospace;",
                                                "{zone.filename}"
                                            }
                                        }
                                    }
                                }
                            }
                            if zones().is_empty() {
                                div { style: "padding:24px; text-align:center; color:{TEXT_DIM};",
                                    "No zone files found in {zdir}. Place .zone files there or set HICKORY_ZONE_DIR."
                                }
                            }
                        }
                    },
                    View::ZoneRecords(_) => rsx! {
                        div { style: "background:{BG_SURFACE}; border-radius:6px; overflow:hidden;",
                            // Table header
                            div {
                                style: "display:grid; grid-template-columns:2fr 80px 3fr 60px; gap:8px; padding:8px 12px; background:{BG_ELEVATED}; font-size:11px; color:{TEXT_DIM}; text-transform:uppercase; letter-spacing:0.05em;",
                                span { "Name" }
                                span { "Type" }
                                span { "Data" }
                                span { "TTL" }
                            }
                            for rec in records().iter() {
                                div {
                                    style: "display:grid; grid-template-columns:2fr 80px 3fr 60px; gap:8px; padding:6px 12px; border-top:1px solid {BORDER}; font-size:12px;",
                                    span { style: "overflow:hidden; text-overflow:ellipsis; white-space:nowrap; color:{TEXT_SECONDARY};",
                                        "{rec.name}"
                                    }
                                    span { style: "color:{type_color(&rec.rtype)}; font-weight:500;",
                                        "{rec.rtype}"
                                    }
                                    span { style: "overflow:hidden; text-overflow:ellipsis; white-space:nowrap; font-family:monospace; font-size:11px;",
                                        "{rec.rdata}"
                                    }
                                    span { style: "color:{TEXT_DIM}; font-size:11px;",
                                        "{rec.ttl}"
                                    }
                                }
                            }
                            if records().is_empty() {
                                div { style: "padding:24px; text-align:center; color:{TEXT_DIM};",
                                    "No records"
                                }
                            }
                        }
                    },
                    View::ZoneEdit(_) => rsx! {
                        textarea {
                            style: "width:100%; height:calc(100vh - 120px); background:{BG_SURFACE}; color:{TEXT_PRIMARY}; border:1px solid {BORDER}; border-radius:6px; padding:12px; font-family:monospace; font-size:12px; resize:none; outline:none;",
                            value: "{edit_content}",
                            oninput: move |e| edit_content.set(e.value()),
                        }
                    },
                }
            }
        }
    }
}

fn type_color(rtype: &str) -> &'static str {
    match rtype {
        "A" | "AAAA" => "#60a5fa",
        "CNAME" => "#a78bfa",
        "MX" => "#f59e0b",
        "TXT" => "#22c55e",
        "NS" => "#f472b6",
        "SOA" => "#94a3b8",
        "SRV" => "#fb923c",
        "PTR" => "#e879f9",
        _ => TEXT_MUTED,
    }
}

// ── Theme ──

const BG_BASE: &str = cosmix_ui::theme::BG_BASE;
const BG_SURFACE: &str = cosmix_ui::theme::BG_SURFACE;
const BG_ELEVATED: &str = cosmix_ui::theme::BG_ELEVATED;
const BORDER: &str = cosmix_ui::theme::BORDER_DEFAULT;
const TEXT_PRIMARY: &str = cosmix_ui::theme::TEXT_PRIMARY;
const TEXT_SECONDARY: &str = cosmix_ui::theme::TEXT_SECONDARY;
const TEXT_MUTED: &str = cosmix_ui::theme::TEXT_MUTED;
const TEXT_DIM: &str = cosmix_ui::theme::TEXT_DIM;

const CSS: &str = r#"
html, body, #main {
    margin: 0; padding: 0;
    width: 100%; height: 100%;
    overflow: hidden;
}
::-webkit-scrollbar { width: 8px; }
::-webkit-scrollbar-track { background: transparent; }
::-webkit-scrollbar-thumb { background: #374151; border-radius: 4px; }
::-webkit-scrollbar-thumb:hover { background: #4b5563; }
button:hover { background: #374151 !important; }
div[style*="cursor:pointer"]:hover { background: #1e293b !important; }
textarea:focus { border-color: #58a6ff !important; }
"#;
