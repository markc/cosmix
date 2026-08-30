//! Captured vendor stdout format shared by the capture/replay executable and
//! the full-run replay tests.

use std::io::{BufRead, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const CAPTURE_VERSION: u32 = 1;

/// One JSONL record in a captured vendor stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "record", rename_all = "snake_case")]
pub enum CaptureRecord {
    Meta {
        version: u32,
        lane: String,
    },
    Line {
        after_ms: u64,
        stdout: String,
    },
    Exit {
        code: Option<i32>,
        signal: Option<i32>,
    },
}

#[derive(Debug, Clone)]
pub struct CapturedLine {
    pub after_ms: u64,
    pub stdout: String,
}

#[derive(Debug, Clone)]
pub struct CapturedStream {
    pub lane: String,
    pub lines: Vec<CapturedLine>,
    pub exit_code: Option<i32>,
    pub exit_signal: Option<i32>,
}

impl CapturedStream {
    /// Load and structurally validate a complete JSONL capture. A capture
    /// interrupted before its exit record is rejected rather than replayed
    /// with an invented status.
    pub fn load(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path)
            .with_context(|| format!("opening replay fixture {}", path.display()))?;
        let mut records = std::io::BufReader::new(file).lines();
        let first = records
            .next()
            .transpose()?
            .context("capture is empty (missing meta record)")?;
        let CaptureRecord::Meta { version, lane } =
            serde_json::from_str(&first).context("decoding capture meta record")?
        else {
            anyhow::bail!("first capture record is not meta");
        };
        anyhow::ensure!(
            version == CAPTURE_VERSION,
            "unsupported capture version {version} (want {CAPTURE_VERSION})"
        );

        let mut lines = Vec::new();
        let mut exit = None;
        let mut duration = Duration::ZERO;
        for (index, raw) in records.enumerate() {
            let raw = raw.with_context(|| format!("reading capture record {}", index + 2))?;
            let record: CaptureRecord = serde_json::from_str(&raw)
                .with_context(|| format!("decoding capture record {}", index + 2))?;
            match record {
                CaptureRecord::Meta { .. } => {
                    anyhow::bail!("capture record {} repeats meta", index + 2)
                }
                CaptureRecord::Line { after_ms, stdout } => {
                    anyhow::ensure!(exit.is_none(), "line follows exit record");
                    anyhow::ensure!(
                        !stdout.contains('\n') && !stdout.contains('\r'),
                        "capture line contains an embedded newline"
                    );
                    duration = duration
                        .checked_add(Duration::from_millis(after_ms))
                        .context("capture line deltas overflow Duration")?;
                    anyhow::ensure!(
                        duration.as_millis() <= i64::MAX as u128,
                        "capture duration exceeds the ledger's i64 millisecond range"
                    );
                    lines.push(CapturedLine { after_ms, stdout });
                }
                CaptureRecord::Exit { code, signal } => {
                    anyhow::ensure!(exit.is_none(), "capture has more than one exit record");
                    anyhow::ensure!(
                        code.is_some() ^ signal.is_some(),
                        "capture exit must contain exactly one of code or signal"
                    );
                    if let Some(code) = code {
                        anyhow::ensure!(
                            (0..=255).contains(&code),
                            "capture exit code {code} is outside 0..=255"
                        );
                    }
                    if let Some(signal) = signal {
                        anyhow::ensure!(signal > 0, "capture exit signal must be positive");
                    }
                    exit = Some((code, signal));
                }
            }
        }
        let (exit_code, exit_signal) =
            exit.context("capture is incomplete (missing exit record)")?;
        Ok(Self {
            lane,
            lines,
            exit_code,
            exit_signal,
        })
    }
}

pub fn write_record(mut out: impl Write, record: &CaptureRecord) -> Result<()> {
    serde_json::to_writer(&mut out, record)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}
