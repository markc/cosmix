//! Vendor-CLI seam for capturing and replaying stdout streams.
//!
//! Capture a real invocation by pointing `FOREMAN_CLAUDE_BIN` or
//! `FOREMAN_CODEX_BIN` here and setting `FOREMAN_CAPTURE_VENDOR_BIN`,
//! `FOREMAN_CAPTURE_FIXTURE`, and `FOREMAN_CAPTURE_LANE`. Replay by pointing
//! the same driver override here and setting `FOREMAN_REPLAY_FIXTURE`.

use std::io::{BufRead, Write};
use std::process::{Command, ExitStatus, Stdio};
use std::time::Instant;

use anyhow::{Context, Result};
use cosmix_foreman::replay::{CAPTURE_VERSION, CaptureRecord, CapturedStream, write_record};

fn main() {
    if let Err(err) = run() {
        eprintln!("foreman-stream-fixture: {err:#}");
        std::process::exit(125);
    }
}

fn run() -> Result<()> {
    if let Some(path) = std::env::var_os("FOREMAN_REPLAY_FIXTURE") {
        return replay(std::path::Path::new(&path));
    }
    let vendor = std::env::var("FOREMAN_CAPTURE_VENDOR_BIN")
        .context("set FOREMAN_REPLAY_FIXTURE, or set FOREMAN_CAPTURE_VENDOR_BIN for capture")?;
    let fixture = std::env::var("FOREMAN_CAPTURE_FIXTURE")
        .context("FOREMAN_CAPTURE_FIXTURE is required for capture")?;
    let lane = std::env::var("FOREMAN_CAPTURE_LANE")
        .context("FOREMAN_CAPTURE_LANE is required for capture")?;
    capture(&vendor, std::path::Path::new(&fixture), &lane)
}

fn replay(path: &std::path::Path) -> Result<()> {
    let capture = CapturedStream::load(path)?;
    let fast = std::env::var("FOREMAN_REPLAY_FAST").is_ok_and(|v| v == "1");
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in capture.lines {
        if !fast {
            std::thread::sleep(std::time::Duration::from_millis(line.after_ms));
        }
        writeln!(out, "{}", line.stdout)?;
        out.flush()?;
    }
    if let Some(code) = capture.exit_code {
        std::process::exit(code);
    }
    #[cfg(unix)]
    if let Some(signal) = capture.exit_signal {
        unsafe {
            libc::raise(signal);
        }
        anyhow::bail!("raising captured signal {signal} returned unexpectedly");
    }
    #[cfg(not(unix))]
    if capture.exit_signal.is_some() {
        anyhow::bail!("signal exits cannot be replayed on this platform");
    }
    Ok(())
}

fn capture(vendor: &str, path: &std::path::Path, lane: &str) -> Result<()> {
    let mut child = Command::new(vendor)
        .args(std::env::args_os().skip(1))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("spawning vendor binary {vendor:?}"))?;
    let mut fixture = std::io::BufWriter::new(
        std::fs::File::create(path)
            .with_context(|| format!("creating capture fixture {}", path.display()))?,
    );
    write_record(
        &mut fixture,
        &CaptureRecord::Meta {
            version: CAPTURE_VERSION,
            lane: lane.to_string(),
        },
    )?;

    let stdout = child.stdout.take().expect("vendor stdout was piped");
    let mut reader = std::io::BufReader::new(stdout);
    let mut raw = String::new();
    let mut last = Instant::now();
    let parent_stdout = std::io::stdout();
    let mut tee = parent_stdout.lock();
    loop {
        raw.clear();
        if reader.read_line(&mut raw)? == 0 {
            break;
        }
        let after_ms = u64::try_from(last.elapsed().as_millis()).unwrap_or(u64::MAX);
        last = Instant::now();
        tee.write_all(raw.as_bytes())?;
        tee.flush()?;
        let stdout = raw.trim_end_matches(['\r', '\n']).to_string();
        write_record(&mut fixture, &CaptureRecord::Line { after_ms, stdout })?;
    }
    let status = child.wait().context("waiting for captured vendor")?;
    let (code, signal) = exit_parts(status);
    write_record(&mut fixture, &CaptureRecord::Exit { code, signal })?;
    exit_like(status)
}

fn exit_parts(status: ExitStatus) -> (Option<i32>, Option<i32>) {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        (status.code(), status.signal())
    }
    #[cfg(not(unix))]
    {
        (status.code(), None)
    }
}

fn exit_like(status: ExitStatus) -> Result<()> {
    let (code, signal) = exit_parts(status);
    if let Some(code) = code {
        std::process::exit(code);
    }
    #[cfg(unix)]
    if let Some(signal) = signal {
        unsafe {
            libc::raise(signal);
        }
    }
    anyhow::bail!("captured vendor ended without an exit code or signal")
}
