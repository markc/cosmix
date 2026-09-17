//! Mixer + roll bake-off, iced arm (ADR 2026-09-17 test C).
//!
//! One view per run: the 64-strip mixer built from `cosmix-iced-widgets`
//! faders, knobs, toggles and meters, or its piano-roll canvas over the dense
//! song. All state comes from `cosmix-bench-feed`, so the Bevy arm draws the
//! same thing from the same numbers. Updates are reactive: the app wakes on
//! window input, and in animated modes once per feed tick (30 Hz), never
//! continuously and never at all when idle.

#[cfg(not(any(feature = "wgpu", feature = "tiny-skia")))]
compile_error!("select exactly one renderer: --features wgpu or --features tiny-skia");
#[cfg(all(feature = "wgpu", feature = "tiny-skia"))]
compile_error!(
    "select exactly one renderer: the arms are measured separately, so wgpu and tiny-skia \
     must not be enabled together"
);

mod app;
mod board;
mod channel;
mod mixer;
mod roll;
mod theme;
mod ticker;

use std::path::PathBuf;
use std::time::Instant;

use cosmix_bench_feed::layout::{WINDOW_HEIGHT, WINDOW_WIDTH};
use cosmix_bench_feed::{BenchSong, DEFAULT_SEED, DEFAULT_STRIPS, Mode};

const APP_ID: &str = "dev.cosmix.bench-mixer-iced";
/// Most channel strips a run accepts (the Bevy arm's limit).
const MAX_STRIPS: usize = 512;
/// Window size limits `--size` accepts, matching the Bevy arm exactly.
const MIN_WINDOW: u32 = 200;
const MAX_WINDOW: u32 = 8000;

/// Which renderer this binary was built with, for the run banner: the two
/// arms are reported separately and a mislabelled run is worthless.
pub const RENDERER: &str = if cfg!(feature = "wgpu") {
    "wgpu"
} else {
    "tiny-skia"
};

const USAGE: &str = "\
usage: cosmix-bench-mixer-iced [options]
  --mode idle|meters|drag|roll   what animates (default idle)
  --view mixer|roll              which surface (default: roll for --mode roll, else mixer)
  --strips N                     channel strips, plus the master (default 64)
  --seed S                       feed seed, decimal or 0x hex (default 0x5eed1234)
  --song PATH                    roll song (default ~/.cache/cosmix-bench/studio-s0/dense-32-track.mid)
  --size WxH                     logical window size (default 1024x576)
  --drag-by script|pointer       drag mode: the feed moves the fader, or an external
                                 pointer does while meters animate (default script)";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Mixer,
    Roll,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub mode: Mode,
    pub view: View,
    pub strips: usize,
    pub seed: u64,
    pub song: Option<PathBuf>,
    /// Logical window size; also the roll's drawing area.
    pub width: u32,
    pub height: u32,
    /// Drag mode only: whether the feed script moves the fader.
    pub scripted_drag: bool,
}

fn parse_seed(text: &str) -> Result<u64, String> {
    let parsed = match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(&hex.replace('_', ""), 16),
        None => text.replace('_', "").parse(),
    };
    parsed.map_err(|_| format!("--seed: not a number: {text:?}"))
}

/// `WxH` in logical pixels, both sides [`MIN_WINDOW`]..=[`MAX_WINDOW`].
fn parse_size(text: &str) -> Result<(u32, u32), String> {
    let bad = || format!("--size must be WxH in {MIN_WINDOW}..={MAX_WINDOW}, got {text:?}");
    let (width, height) = text.split_once(['x', 'X']).ok_or_else(bad)?;
    let side = |value: &str| {
        value
            .parse::<u32>()
            .ok()
            .filter(|n| (MIN_WINDOW..=MAX_WINDOW).contains(n))
            .ok_or_else(bad)
    };
    Ok((side(width)?, side(height)?))
}

pub fn parse_args(args: &[String]) -> Result<Config, String> {
    let mut mode = Mode::Idle;
    let mut view = None;
    let mut strips = DEFAULT_STRIPS;
    let mut seed = DEFAULT_SEED;
    let mut song = None;
    let mut size = (WINDOW_WIDTH, WINDOW_HEIGHT);
    let mut scripted_drag = true;
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        if flag == "--help" || flag == "-h" {
            return Err(USAGE.to_owned());
        }
        let value = iter
            .next()
            .ok_or_else(|| format!("{flag} needs a value\n{USAGE}"))?;
        match flag.as_str() {
            "--mode" => mode = value.parse()?,
            "--view" => {
                view = Some(match value.as_str() {
                    "mixer" => View::Mixer,
                    "roll" => View::Roll,
                    other => return Err(format!("unknown view {other:?} (mixer|roll)")),
                })
            }
            "--strips" => {
                strips = value
                    .parse()
                    .ok()
                    .filter(|n| (1..=MAX_STRIPS).contains(n))
                    .ok_or_else(|| format!("--strips must be 1..={MAX_STRIPS}, got {value:?}"))?
            }
            "--seed" => seed = parse_seed(value)?,
            "--song" => song = Some(PathBuf::from(value)),
            "--size" => size = parse_size(value)?,
            "--drag-by" => {
                scripted_drag = match value.as_str() {
                    "script" => true,
                    "pointer" => false,
                    other => return Err(format!("unknown --drag-by {other:?} (script|pointer)")),
                }
            }
            other => return Err(format!("unknown option {other:?}\n{USAGE}")),
        }
    }
    let view = view.unwrap_or(if mode == Mode::Roll {
        View::Roll
    } else {
        View::Mixer
    });
    match (mode, view) {
        (Mode::Meters | Mode::Drag, View::Roll) => {
            return Err(format!("--mode {mode} animates the mixer; use --view mixer"));
        }
        (Mode::Roll, View::Mixer) => {
            return Err("--mode roll animates the roll; use --view roll".to_owned());
        }
        _ => {}
    }
    let song = match view {
        View::Roll => Some(
            song.or_else(cosmix_bench_feed::song::default_song_path)
                .ok_or("--song not given and $HOME is unset")?,
        ),
        View::Mixer => None,
    };
    Ok(Config {
        mode,
        view,
        strips,
        seed,
        song,
        width: size.0,
        height: size.1,
        scripted_drag,
    })
}

fn main() -> iced::Result {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let config = parse_args(&args).unwrap_or_else(|message| {
        eprintln!("{message}");
        std::process::exit(2);
    });
    eprintln!(
        "bench-mixer-iced: renderer={RENDERER} mode={} view={:?} strips={} seed={:#x} \
         size={}x{} song={}",
        config.mode,
        config.view,
        config.strips,
        config.seed,
        config.width,
        config.height,
        config
            .song
            .as_ref()
            .map_or_else(|| "-".to_owned(), |p| p.display().to_string()),
    );
    let song = config.song.as_ref().map(|path| {
        let started = Instant::now();
        let song = BenchSong::load(path).unwrap_or_else(|error| {
            eprintln!("bench-mixer-iced: {error}");
            std::process::exit(1);
        });
        eprintln!(
            "bench-mixer-iced: loaded {} notes, {} tracks, {} ticks in {:.1} ms",
            song.notes.len(),
            song.track_count,
            song.length_ticks,
            started.elapsed().as_secs_f64() * 1000.0
        );
        roll::report_note_cap(&song);
        song
    });
    let tokens = theme::tokens().unwrap_or_else(|error| {
        eprintln!("bench-mixer-iced: {error}");
        std::process::exit(1);
    });
    let title = format!(
        "Mixer bench (iced {RENDERER}) - {} - {}",
        config.mode,
        match config.view {
            View::Mixer => "mixer",
            View::Roll => "roll",
        }
    );
    let size = iced::Size::new(config.width as f32, config.height as f32);
    let iced_theme = theme::iced_theme(tokens);

    iced::application(
        move || app::Bench::new(config.clone(), song.clone(), tokens),
        app::Bench::update,
        app::Bench::view,
    )
    .title(move |_: &app::Bench| title.clone())
    .subscription(app::Bench::subscription)
    // Built once: `Theme::custom` generates a whole extended palette, and
    // iced asks for the theme on every redraw.
    .theme(move |_: &app::Bench| iced_theme.clone())
    .window(iced::window::Settings {
        size,
        min_size: Some(size),
        max_size: Some(size),
        resizable: false,
        platform_specific: iced::window::settings::PlatformSpecific {
            application_id: APP_ID.to_owned(),
            ..Default::default()
        },
        ..Default::default()
    })
    .run()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn defaults_are_an_idle_mixer_at_the_shared_window_size() {
        let config = parse_args(&[]).unwrap();
        assert_eq!(config.mode, Mode::Idle);
        assert_eq!(config.view, View::Mixer);
        assert_eq!(config.strips, 64);
        assert_eq!(config.seed, DEFAULT_SEED);
        assert_eq!(config.song, None);
        assert_eq!((config.width, config.height), (WINDOW_WIDTH, WINDOW_HEIGHT));
        assert!(config.scripted_drag);
    }

    #[test]
    fn roll_mode_picks_the_roll_view_and_a_song() {
        let config = parse_args(&args(&["--mode", "roll", "--song", "/x/y.mid"])).unwrap();
        assert_eq!(config.view, View::Roll);
        assert_eq!(config.song, Some(PathBuf::from("/x/y.mid")));
        let idle_roll = parse_args(&args(&["--view", "roll", "--song", "a.mid"])).unwrap();
        assert_eq!((idle_roll.mode, idle_roll.view), (Mode::Idle, View::Roll));
    }

    #[test]
    fn options_parse() {
        let config = parse_args(&args(&[
            "--mode",
            "drag",
            "--strips",
            "8",
            "--seed",
            "0xdead_beef",
            "--size",
            "1024x576",
            "--drag-by",
            "pointer",
        ]))
        .unwrap();
        assert_eq!(config.mode, Mode::Drag);
        assert_eq!(config.strips, 8);
        assert_eq!(config.seed, 0xdead_beef);
        assert_eq!((config.width, config.height), (1024, 576));
        assert!(!config.scripted_drag);
        assert_eq!(parse_seed("42"), Ok(42));
        assert_eq!(parse_size("1600X900"), Ok((1600, 900)));
    }

    #[test]
    fn bad_input_is_refused() {
        for bad in [
            &["--mode", "fast"][..],
            &["--view", "tree"],
            &["--strips", "0"],
            &["--strips", "513"],
            &["--seed", "0xzz"],
            &["--size", "1024"],
            &["--size", "0x576"],
            &["--size", "1024x"],
            &["--size", "9000x576"],
            &["--size", "199x576"],
            &["--size", "-1x5"],
            &["--mode"],
            &["--frobnicate", "1"],
            &["--mode", "meters", "--view", "roll"],
            &["--mode", "roll", "--view", "mixer"],
            &["--drag-by", "wind"],
            &["--help"],
        ] {
            assert!(parse_args(&args(bad)).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn exactly_one_renderer_is_built_in() {
        // The compile_error! guards above make the other combinations
        // unbuildable; this pins the label the run banner and the reported
        // arm name come from.
        assert!(matches!(RENDERER, "wgpu" | "tiny-skia"));
    }
}
