//! End-to-end tests for the pure `midi2` command surface.

use std::{
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

use cosmix_musicd::midi2::{
    cv2::Midi2Cv,
    msg::{Message, SysEx7, SysEx7Format, Utility},
    smfio::{self, TimedMessage, Timeline},
    umpfile::{self, Tempo},
};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct TempPath(PathBuf);

impl TempPath {
    fn new(extension: &str) -> Self {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!(
            "cosmix-musicd-midi2-{}-{sequence}.{extension}",
            std::process::id()
        )))
    }
}

impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn fixtures() -> Vec<PathBuf> {
    let directory = manifest_dir().join("tests/fixtures");
    let mut paths = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("mid"))
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

fn raw_cosump(words: &[u32]) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(umpfile::MAGIC);
    bytes.extend_from_slice(&28u32.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&(words.len() as u64).to_le_bytes());
    for word in words {
        bytes.extend_from_slice(&word.to_le_bytes());
    }
    bytes
}

fn tick_spanning_sysex_fixture() -> Vec<u8> {
    let track = [0, 0xF0, 1, 1, 10, 0xF7, 2, 2, 0xF7, 0, 0xFF, 0x2F, 0];
    cosmix_musicd::smf::assemble_format0([1, 0xE0], &track)
}

#[test]
fn every_fixture_passes_the_library_semantic_roundtrip() {
    let fixtures = fixtures();
    assert!(!fixtures.is_empty());
    for path in fixtures {
        let bytes = std::fs::read(&path).unwrap();
        let report = smfio::roundtrip_smf(&bytes, None)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        assert_eq!(
            report.first_divergence(),
            None,
            "fixture {}",
            path.display()
        );
        assert_eq!(report.dropped, Default::default());
    }

    let report = smfio::roundtrip_smf(&tick_spanning_sysex_fixture(), None).unwrap();
    assert_eq!(
        report.first_divergence(),
        None,
        "tick-spanning SysEx fixture"
    );
    assert_eq!(report.dropped.sysex_timing, 1);
}

#[test]
fn cosump_file_round_trips_through_a_real_temp_path() {
    let timeline = Timeline {
        ticks_per_quarter: 960,
        tempos: vec![
            Tempo {
                absolute_tick: 0,
                us_per_quarter: 500_000,
            },
            Tempo {
                absolute_tick: 1920,
                us_per_quarter: 375_000,
            },
        ],
        events: vec![
            TimedMessage {
                tick: 0,
                message: Message::Midi2Cv(Midi2Cv::program_change(0, 1, 10, Some(0x1234))),
            },
            TimedMessage {
                tick: 0x10_0005,
                message: Message::Midi2Cv(Midi2Cv::note_on(0, 1, 60, 0x8000, 0, 0)),
            },
        ],
    };
    let file = smfio::to_ump_file(&timeline).unwrap();
    let bytes = umpfile::write(&file).unwrap();
    let path = TempPath::new("cosump");
    std::fs::write(&path.0, bytes).unwrap();
    let decoded = umpfile::read(&std::fs::read(&path.0).unwrap()).unwrap();
    assert_eq!(decoded, file);
    assert_eq!(smfio::from_ump_file(&decoded).unwrap(), timeline);
}

#[test]
fn dump_output_snapshot_is_exact() {
    let timeline = Timeline {
        ticks_per_quarter: 480,
        tempos: Vec::new(),
        events: vec![
            TimedMessage {
                tick: 0,
                message: Message::Midi2Cv(Midi2Cv::note_on(2, 3, 60, 0x8000, 0, 0)),
            },
            TimedMessage {
                tick: 240,
                message: Message::Midi2Cv(Midi2Cv::control_change(2, 3, 7, u32::MAX)),
            },
        ],
    };
    assert_eq!(
        smfio::dump_lines(&timeline).collect::<Vec<_>>().join("\n"),
        "tick=0 group=2 channel=3 note-on note=60 velocity=32768 velocity-pct=50.00% attribute-type=0 attribute-data=0\n\
         tick=240 group=2 channel=3 control-change controller=7 value=4294967295 value-pct=100.00%"
    );
}

#[test]
fn built_binary_roundtrip_command_accepts_a_fixture() {
    let fixture = fixtures().into_iter().next().expect("at least one fixture");
    let output = Command::new(env!("CARGO_BIN_EXE_cosmix-musicd"))
        .args(["midi2", "roundtrip"])
        .arg(&fixture)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("roundtrip ok:"));
    assert!(output.stderr.is_empty());
}

#[test]
fn built_binary_up_dump_down_and_invalid_track_paths() {
    let fixture = fixtures().into_iter().next().expect("at least one fixture");
    let cosump = TempPath::new("cosump");
    let midi = TempPath::new("mid");
    let binary = env!("CARGO_BIN_EXE_cosmix-musicd");

    let up = Command::new(binary)
        .args(["midi2", "up"])
        .arg(&fixture)
        .arg(&cosump.0)
        .output()
        .unwrap();
    assert!(
        up.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&up.stderr)
    );
    assert!(cosump.0.is_file());

    let dump = Command::new(binary)
        .args(["midi2", "dump"])
        .arg(&cosump.0)
        .output()
        .unwrap();
    assert!(dump.status.success());
    assert!(String::from_utf8_lossy(&dump.stdout).contains("tick="));

    let down = Command::new(binary)
        .args(["midi2", "down"])
        .arg(&cosump.0)
        .arg(&midi.0)
        .output()
        .unwrap();
    assert!(
        down.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&down.stderr)
    );
    assert!(down.stderr.is_empty(), "lossless down must be silent");
    assert!(midi.0.is_file());

    let invalid = Command::new(binary)
        .args(["midi2", "up"])
        .arg(&fixture)
        .arg(&cosump.0)
        .args(["--track", "999"])
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("out of range"));
}

#[test]
fn built_binary_down_reports_each_real_loss() {
    let cosump = TempPath::new("cosump");
    let midi = TempPath::new("mid");
    let timeline = Timeline {
        ticks_per_quarter: 480,
        tempos: Vec::new(),
        events: vec![
            TimedMessage {
                tick: 0,
                message: Message::Midi2Cv(Midi2Cv::per_note_pitch_bend(0, 0, 60, 1)),
            },
            TimedMessage {
                tick: 0,
                message: Message::Midi2Cv(Midi2Cv::note_on(1, 0, 60, 0x8000, 0, 0)),
            },
        ],
    };
    let encoded = umpfile::write(&smfio::to_ump_file(&timeline).unwrap()).unwrap();
    std::fs::write(&cosump.0, encoded).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_cosmix-musicd"))
        .args(["midi2", "down"])
        .arg(&cosump.0)
        .arg(&midi.0)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "dropped per-note-pitch-bend: 1\ndropped group-routing: 1\n"
    );
}

#[test]
fn built_binary_down_rejects_forty_byte_cosump_with_standalone_sysex7_end() {
    let cosump = TempPath::new("cosump");
    let midi = TempPath::new("mid");
    let mut words = Utility::delta_clockstamp_tpq(480).encode().words().to_vec();
    words.extend_from_slice(
        SysEx7::new(0, SysEx7Format::End, &[1])
            .unwrap()
            .encode()
            .words(),
    );
    let bytes = raw_cosump(&words);
    assert_eq!(bytes.len(), 40);
    std::fs::write(&cosump.0, bytes).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_cosmix-musicd"))
        .args(["midi2", "down"])
        .arg(&cosump.0)
        .arg(&midi.0)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("standalone SysEx7"));
    assert!(!midi.0.exists());
}

#[test]
fn tick_spanning_sysex_cli_roundtrip_matches_real_up_down_projection() {
    let input = TempPath::new("mid");
    let cosump = TempPath::new("cosump");
    let output = TempPath::new("mid");
    std::fs::write(&input.0, tick_spanning_sysex_fixture()).unwrap();
    let binary = env!("CARGO_BIN_EXE_cosmix-musicd");

    let roundtrip = Command::new(binary)
        .args(["midi2", "roundtrip"])
        .arg(&input.0)
        .output()
        .unwrap();
    assert!(
        roundtrip.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&roundtrip.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&roundtrip.stderr),
        "dropped sysex-timing: 1\n"
    );

    let up = Command::new(binary)
        .args(["midi2", "up"])
        .arg(&input.0)
        .arg(&cosump.0)
        .output()
        .unwrap();
    assert!(up.status.success());
    let down = Command::new(binary)
        .args(["midi2", "down"])
        .arg(&cosump.0)
        .arg(&output.0)
        .output()
        .unwrap();
    assert!(down.status.success());
    assert_eq!(
        String::from_utf8_lossy(&down.stderr),
        "dropped sysex-timing: 1\n"
    );

    let reimported = smfio::import_smf(&std::fs::read(&output.0).unwrap(), None).unwrap();
    assert!(matches!(
        reimported.events.as_slice(),
        [TimedMessage {
            tick: 0,
            message: Message::SysEx7(SysEx7::Complete(data)),
        }] if data.data() == [1, 2]
    ));
}

#[test]
fn built_binary_roundtrip_returns_failure_for_invalid_input() {
    let invalid = TempPath::new("mid");
    std::fs::write(&invalid.0, b"MThd").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_cosmix-musicd"))
        .args(["midi2", "roundtrip"])
        .arg(&invalid.0)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("SMF parse failed"));
}

/// Run-time manifest directory rather than the `env!`-baked one: cargo exports
/// `CARGO_MANIFEST_DIR` into the test process, and that names the tree cargo is
/// actually running in, whereas `env!` records whichever tree last *compiled*
/// the binary. The two diverge when one `CARGO_TARGET_DIR` is shared across
/// several git worktrees of this repo — cargo writes workspace-relative paths
/// into its dep-info, so an artefact built in a sibling worktree is judged
/// fresh and rerun here, still pointing at that tree's fixtures. Falls back to
/// the compile-time value when the binary is run outside cargo.
fn manifest_dir() -> std::path::PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")))
}
