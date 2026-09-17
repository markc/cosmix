//! Mixer + roll bake-off, Bevy/CTK arm (ADR 2026-09-17 test C).
//!
//! One view per run: the 64-strip mixer built from CTK's faders, knobs,
//! toggles and meters, or a pooled piano roll of the dense song. All state
//! comes from `cosmix-bench-feed`, so the iced arm draws the same thing.
//! Updates are reactive: the app wakes on window input, and in animated
//! modes on each feed tick (30 Hz), never continuously.

mod mixer_view;
mod roll_view;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use bevy::feathers::{dark_theme::create_dark_theme, theme::UiTheme};
use bevy::prelude::*;
use bevy::winit::{UpdateMode, WinitSettings};
use cosmix_bench_feed::layout::{WINDOW_HEIGHT, WINDOW_WIDTH};
use cosmix_bench_feed::{BenchSong, DEFAULT_SEED, DEFAULT_STRIPS, Mode, TICK_HZ, tick_at};
use ctk::prelude::{
    CtkThemeMetrics, CtkThemePlugin, CtkWidgetsPlugin, FeathersPlugins, Mode as ThemeMode,
    Scheme, ThemeSpec, ThemeState, apply_theme,
};

const APP_ID: &str = "dev.cosmix.bench-mixer-bevy";
/// Most channel strips a run accepts.
const MAX_STRIPS: usize = 512;
/// The app redraws continuously for this long after start so fonts and the
/// first layout settle before the reactive schedule takes over.
const WARMUP: Duration = Duration::from_secs(1);
/// Wake this long after a tick boundary, so the woken frame sees the new tick.
const TICK_SLACK: Duration = Duration::from_micros(500);

const USAGE: &str = "\
usage: cosmix-bench-mixer-bevy [options]
  --mode idle|meters|drag|roll   what animates (default idle)
  --view mixer|roll              which surface (default: roll for --mode roll, else mixer)
  --strips N                     channel strips, plus the master (default 64)
  --seed S                       feed seed, decimal or 0x hex (default 0x5eed1234)
  --song PATH                    roll song (default ~/.cache/cosmix-bench/studio-s0/dense-32-track.mid)
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

pub fn parse_args(args: &[String]) -> Result<Config, String> {
    let mut mode = Mode::Idle;
    let mut view = None;
    let mut strips = DEFAULT_STRIPS;
    let mut seed = DEFAULT_SEED;
    let mut song = None;
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
        scripted_drag,
    })
}

/// The run's configuration and clock.
#[derive(Resource)]
pub struct Bench {
    pub config: Config,
    started: Option<Instant>,
    /// Run time at the start of the current frame.
    frame_elapsed: Duration,
    /// Tests pin the tick instead of reading the wall clock.
    pub forced_tick: Option<u64>,
}

impl Bench {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            started: None,
            frame_elapsed: Duration::ZERO,
            forced_tick: None,
        }
    }

    /// The feed tick this frame shows.
    pub fn tick(&self) -> u64 {
        self.forced_tick.unwrap_or_else(|| tick_at(self.frame_elapsed))
    }
}

/// Stamp the frame start; the first frame starts the run clock.
fn frame_clock(mut bench: ResMut<Bench>) {
    let started = *bench.started.get_or_insert_with(Instant::now);
    bench.frame_elapsed = started.elapsed();
}

/// Reactive scheduling: continuous during warm-up, then wake on input only,
/// plus once per feed tick in animated modes. Winit measures the wait from the
/// start of this frame, so the next wake is aimed from the frame stamp.
fn schedule_wakes(bench: Res<Bench>, mut winit: ResMut<WinitSettings>) {
    let elapsed = bench.frame_elapsed;
    let mode = if elapsed < WARMUP {
        UpdateMode::Continuous
    } else if bench.config.mode.is_animated() {
        let next = Duration::from_secs_f64((tick_at(elapsed) + 1) as f64 / f64::from(TICK_HZ));
        UpdateMode::reactive_low_power(next.saturating_sub(elapsed) + TICK_SLACK)
    } else {
        UpdateMode::reactive_low_power(Duration::from_secs(3600))
    };
    if winit.focused_mode != mode {
        winit.focused_mode = mode;
        winit.unfocused_mode = mode;
    }
}

/// Everything except the window and renderer, shared with the headless tests.
pub fn add_bench_plugins(app: &mut App, config: Config, song: Option<BenchSong>) {
    let view = config.view;
    app.add_plugins((CtkThemePlugin::new(None), CtkWidgetsPlugin))
        .insert_resource(Bench::new(config))
        .insert_resource(WinitSettings {
            focused_mode: UpdateMode::Continuous,
            unfocused_mode: UpdateMode::Continuous,
        })
        .add_systems(Startup, (setup_theme, setup_camera))
        .add_systems(First, frame_clock)
        .add_systems(Last, schedule_wakes);
    match view {
        View::Mixer => app.add_plugins(mixer_view::MixerViewPlugin),
        View::Roll => app.add_plugins(roll_view::RollViewPlugin {
            song: song.expect("the roll view needs a song"),
        }),
    };
}

/// A fixed palette (Ocean dark), not the user's theme files, so every run
/// and every arm draws the same colours.
fn setup_theme(
    mut theme: ResMut<UiTheme>,
    mut theme_state: ResMut<ThemeState>,
    mut metrics: ResMut<CtkThemeMetrics>,
) {
    *theme = UiTheme(create_dark_theme());
    let spec = ThemeSpec::from_scheme(Scheme::Ocean, ThemeMode::Dark);
    *metrics = spec.metrics.clone();
    apply_theme(&mut theme, &mut theme_state, &spec);
}

fn setup_camera(mut commands: Commands) {
    commands.spawn(Camera2d);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let config = parse_args(&args).unwrap_or_else(|message| {
        eprintln!("{message}");
        std::process::exit(2);
    });
    eprintln!(
        "bench-mixer-bevy: mode={} view={:?} strips={} seed={:#x} song={}",
        config.mode,
        config.view,
        config.strips,
        config.seed,
        config
            .song
            .as_ref()
            .map_or_else(|| "-".to_owned(), |p| p.display().to_string()),
    );
    let song = config.song.as_ref().map(|path| {
        let started = Instant::now();
        let song = BenchSong::load(path).unwrap_or_else(|error| {
            eprintln!("bench-mixer-bevy: {error}");
            std::process::exit(1);
        });
        eprintln!(
            "bench-mixer-bevy: loaded {} notes, {} tracks, {} ticks in {:.1} ms",
            song.notes.len(),
            song.track_count,
            song.length_ticks,
            started.elapsed().as_secs_f64() * 1000.0
        );
        song
    });
    let title = format!(
        "Mixer bench (bevy) - {} - {}",
        config.mode,
        match config.view {
            View::Mixer => "mixer",
            View::Roll => "roll",
        }
    );

    let mut app = App::new();
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(Window {
            title,
            name: Some(APP_ID.to_owned()),
            resolution: (WINDOW_WIDTH, WINDOW_HEIGHT).into(),
            resizable: false,
            ..default()
        }),
        ..default()
    }))
    .add_plugins(FeathersPlugins);
    add_bench_plugins(&mut app, config, song);
    app.run();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn defaults_are_an_idle_mixer() {
        let config = parse_args(&[]).unwrap();
        assert_eq!(config.mode, Mode::Idle);
        assert_eq!(config.view, View::Mixer);
        assert_eq!(config.strips, 64);
        assert_eq!(config.seed, DEFAULT_SEED);
        assert_eq!(config.song, None);
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
            "--mode", "drag", "--strips", "8", "--seed", "0xdead_beef", "--drag-by", "pointer",
        ]))
        .unwrap();
        assert_eq!(config.mode, Mode::Drag);
        assert_eq!(config.strips, 8);
        assert_eq!(config.seed, 0xdead_beef);
        assert!(!config.scripted_drag);
        assert_eq!(parse_seed("42"), Ok(42));
    }

    #[test]
    fn bad_input_is_refused() {
        for bad in [
            &["--mode", "fast"][..],
            &["--view", "tree"],
            &["--strips", "0"],
            &["--strips", "513"],
            &["--seed", "0xzz"],
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
    fn forced_tick_overrides_the_clock() {
        let mut bench = Bench::new(parse_args(&[]).unwrap());
        assert_eq!(bench.tick(), 0);
        bench.forced_tick = Some(99);
        assert_eq!(bench.tick(), 99);
    }
}
