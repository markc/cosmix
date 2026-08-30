//! The `musicd` mixer board with a shared menu bar over three switchable
//! views — Mixer (32 strips + master), Waves (offline-rendered master
//! waveform), Piano Roll — plus a transport footer (RTZ / Play / Stop, live
//! scrubber, `M:SS / M:SS` readout) and a song-metadata footer.

mod action;
mod app_port;
mod editor;
mod file_io;
mod settings;
mod song_bus;
mod song_load;
mod transport;
mod transport_bus;
mod views;

use std::fs;
use std::io;
use std::os::unix::fs as unix_fs;
use std::path::{Path, PathBuf};

use bevy::asset::AssetPlugin;
use bevy::feathers::{dark_theme::create_dark_theme, theme::UiTheme};
use bevy::prelude::*;
use bevy::winit::{UpdateMode, WinitSettings};
use cosmix_mixer_schema::NUM_CHANNELS;
use ctk::prelude::*;
use transport::{InProcessSource, InProcessTransport};
use views::{ActiveView, ViewRoot, WavesContainer};

pub(crate) const IDENTITY: AppIdentity = AppIdentity {
    slug: "studio",
    display_name: "CosMix Studio",
};
const LEGACY_APP_SLUG: &str = "midiseq";
const LEGACY_MIGRATION_MARKER: &str = ".midiseq-to-studio-v1";

/// The scrubber travel width; the rest of the footer (buttons + readout) sits
/// to its sides inside the ~1980px window.
const SCRUBBER_WIDTH: f32 = 1300.0;

/// Which smoke harness (if any) the board was launched with; consumed at
/// setup to insert the matching [`smoke`] run resource on channel 0's fader.
#[derive(Resource, Clone, Copy, Default)]
struct SmokeFlags {
    write: bool,
    stream: bool,
}

/// `--autoplay`: press Play once the pipeline is ready — through the SAME
/// activation path a pointer click takes (`Activate` on the transport Play
/// button, which the ctk action-button observer turns into the final
/// `ControlChange`), so playback starts via an ordinary revisioned write,
/// not a side-channel. The unattended-capture analogue of the daemon's
/// `--autoplay` (which seeds RT state directly; this is the more honest
/// form, and the fused snapshot still reports `benchmark_eligible=false`).
#[derive(Resource)]
struct Autoplay {
    pressed: bool,
}

// KNOWN LIMITATION (deferred, Phase-0): one-shot autoplay latches `pressed`
// when it emits Play. If the mixer FIRST becomes ready on the same frame a file
// load emits `AudioIntent::Reset`, Reset dominance (see `reduce_audio_intents`)
// drops this Play and autoplay never retries — the song loads stopped.
// Unreachable today: `--song --autoplay` loads before scheduling starts, and
// user file-loads happen long after first-ready. If an automatic startup
// file-load is ever added, make autoplay retry until it observes `playing`
// (bounded) instead of latching on emit.
fn autoplay(
    mut audio: bevy::ecs::message::MessageWriter<action::AudioIntent>,
    state: Res<MusicdMixerState>,
    run: Option<ResMut<Autoplay>>,
) {
    let Some(mut run) = run else { return };
    if run.pressed || state.connection != MixerConnectionState::Connected || !state.ready {
        return;
    }
    run.pressed = true;
    // Through the same intent bus a Space press takes.
    audio.write(action::AudioIntent::Play);
}

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let noded_url = app_port::parse_noded_url(&args)
        .unwrap_or_else(|error| {
            eprintln!("studio: {error}");
            std::process::exit(2);
        })
        .unwrap_or_else(ctk::prelude::resolve_noded_url);
    let source = parse_source(&args).unwrap_or_else(|error| {
        eprintln!("studio: {error}");
        std::process::exit(2);
    });
    // Remote (Bus app.song.load) file authority: owner-configured roots, none
    // by default (remote loads denied). The local picker is unaffected.
    let song_policy = song_bus::SongBusPolicy::from_args(&args);
    let smoke_flags = SmokeFlags {
        write: args.iter().any(|arg| arg == "--smoke-write"),
        stream: args.iter().any(|arg| arg == "--smoke-stream"),
    };
    let autoplay_enabled = args.iter().any(|arg| arg == "--autoplay");
    // `--view mixer|waves|roll`: the starting view (default mixer).
    let initial_view = match args
        .iter()
        .position(|arg| arg == "--view")
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
    {
        None | Some("mixer") => ActiveView::Mixer,
        Some("waves") => ActiveView::Waves,
        Some("roll") => ActiveView::PianoRoll,
        Some(other) => {
            eprintln!("studio: unknown --view {other:?} (mixer|waves|roll)");
            std::process::exit(2);
        }
    };
    let (transport, edit_handle, stem_waves) =
        InProcessTransport::new(source).unwrap_or_else(|error| {
            eprintln!("studio: {error}");
            std::process::exit(1);
        });

    let app_dirs = AppDirs::resolve(IDENTITY.slug).unwrap_or_else(|| {
        eprintln!("studio: no absolute app-data root is available");
        std::process::exit(1);
    });
    migrate_legacy_app_dir(&app_dirs);
    let asset_root = prepare_data_root(&app_dirs).unwrap_or_else(|error| {
        eprintln!("studio: {error}");
        app_dirs.cache()
    });
    let theme_dir = app_dirs.config();

    let mut app = App::new();
    app.add_plugins(
        DefaultPlugins
            .set(AssetPlugin {
                file_path: asset_root.to_string_lossy().into_owned(),
                ..default()
            })
            .set(WindowPlugin {
                primary_window: Some(Window {
                    title: format!("{} · musicd mixer", IDENTITY.display_name),
                    name: Some(IDENTITY.app_id()),
                    resolution: (1980, 1080).into(),
                    resizable: true,
                    ..default()
                }),
                ..default()
            }),
    )
    .add_plugins((SvgPlugin, FeathersPlugins));
    add_runtime_plugins(&mut app, Some(theme_dir), noded_url, Box::new(transport));
    app.insert_resource(smoke_flags)
        .insert_resource(AutoplayEnabled(autoplay_enabled))
        .insert_resource(song_policy)
        .insert_resource(initial_view)
        // Reactive rendering: idle at ~5 Hz (redraw on input, or a 200 ms heartbeat
        // to drain Bus/async state), CONTINUOUS while playing — see tune_update_mode.
        .insert_resource(WinitSettings {
            focused_mode: UpdateMode::reactive(std::time::Duration::from_millis(200)),
            unfocused_mode: UpdateMode::reactive_low_power(std::time::Duration::from_secs(1)),
        })
        .add_systems(Startup, setup)
        .add_systems(
            Update,
            (autoplay.in_set(action::ActionProduce), tune_update_mode),
        );

    // A --stems launch gets its arranger lanes up-front (pyramids folded at
    // load, before the bank moved onto the RT thread).
    if let Some(mut waves) = stem_waves {
        // The region-edit loop (document + undo + live engine swap).
        if let Some(parts) = waves.edit.take() {
            app.insert_resource(views::RegionEditor::new(parts));
        }
        app.insert_resource(views::WaveLanes::from_stems(waves));
    }
    // A --song (or bare) launch gets the piano-roll + editing loop.
    if let Some(handle) = edit_handle {
        // Startup indication: what the session opened with.
        let is_empty = handle
            .song
            .tracks()
            .iter()
            .all(|track| track.notes().is_empty());
        let startup_status = match (&handle.soundfont_source, is_empty) {
            (Some(sf), true) => format!(
                "empty session - File > Open Song... (soundfont {})",
                sf.file_name().unwrap_or_default().to_string_lossy()
            ),
            (None, true) => {
                "empty session, no soundfont - File > Open Song / Open SoundFont".to_string()
            }
            (Some(sf), false) => format!(
                "song loaded (soundfont {})",
                sf.file_name().unwrap_or_default().to_string_lossy()
            ),
            (None, false) => "song loaded, NO soundfont - silent until one is opened".to_string(),
        };
        app.insert_resource(views::StartupStatus(startup_status));
        app.add_plugins((PianoRollPlugin, editor::SongEditorPlugin))
            .insert_resource(editor::SongEditor::new(handle));
    }
    app.run();
}

fn migrate_legacy_app_dir(studio: &AppDirs) {
    let Some(legacy) = AppDirs::resolve(LEGACY_APP_SLUG) else {
        return;
    };
    match migrate_legacy_root(studio.root(), legacy.root()) {
        Ok(LegacyMigration::Migrated(method)) => eprintln!(
            "studio: migrated legacy app data {} -> {} ({method})",
            legacy.root().display(),
            studio.root().display(),
        ),
        Ok(LegacyMigration::Conflict) => {
            eprintln!("{}", legacy_migration_warning(studio.root(), legacy.root()))
        }
        Ok(
            LegacyMigration::NotApplicable
            | LegacyMigration::AlreadyHandled
            | LegacyMigration::MarkedCurrentRoot,
        ) => {}
        Err(error) => eprintln!(
            "studio: could not migrate legacy app data {} -> {}: {error}",
            legacy.root().display(),
            studio.root().display(),
        ),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum LegacyMigration {
    NotApplicable,
    AlreadyHandled,
    MarkedCurrentRoot,
    Migrated(&'static str),
    Conflict,
}

fn legacy_migration_marker(studio: &Path) -> PathBuf {
    studio.join(LEGACY_MIGRATION_MARKER)
}

fn write_legacy_migration_marker(studio: &Path) -> io::Result<()> {
    fs::write(
        legacy_migration_marker(studio),
        b"legacy midiseq app-root migration handled\n",
    )
}

fn legacy_migration_warning(studio: &Path, legacy: &Path) -> String {
    format!(
        "studio: legacy migration requires manual resolution because both {} and {} exist. \
Automatic merge is disabled to avoid clobbering state. Back up both roots, then either move \
the current Studio root aside and restart to migrate the legacy root, or merge the wanted \
files manually and create {} to acknowledge completion.",
        legacy.display(),
        studio.display(),
        legacy_migration_marker(studio).display(),
    )
}

/// One-shot slug migration. A direct rename is atomic; cross-filesystem or
/// otherwise unsupported renames fall back to a staged recursive copy.
fn migrate_legacy_root(studio: &Path, legacy: &Path) -> io::Result<LegacyMigration> {
    if studio == legacy {
        return Ok(LegacyMigration::NotApplicable);
    }
    if legacy_migration_marker(studio).exists() {
        return Ok(LegacyMigration::AlreadyHandled);
    }
    if studio.exists() {
        if legacy.exists() {
            return Ok(LegacyMigration::Conflict);
        }
        write_legacy_migration_marker(studio)?;
        return Ok(LegacyMigration::MarkedCurrentRoot);
    }
    if !legacy.exists() {
        fs::create_dir_all(studio)?;
        write_legacy_migration_marker(studio)?;
        return Ok(LegacyMigration::MarkedCurrentRoot);
    }
    match fs::rename(legacy, studio) {
        Ok(()) => {
            write_legacy_migration_marker(studio)?;
            return Ok(LegacyMigration::Migrated("rename"));
        }
        Err(rename_error) => {
            eprintln!("studio: legacy app-data rename unavailable ({rename_error}); copying");
        }
    }

    let parent = studio.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "studio app root has no parent")
    })?;
    fs::create_dir_all(parent)?;
    let file_name = studio
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "studio app root has no UTF-8 final component",
            )
        })?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let staging = parent.join(format!(
        ".{file_name}.migration-{}-{nonce}",
        std::process::id()
    ));
    if let Err(error) = copy_tree(legacy, &staging) {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    if let Err(error) = fs::rename(&staging, studio) {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    fs::remove_dir_all(legacy)?;
    write_legacy_migration_marker(studio)?;
    Ok(LegacyMigration::Migrated("copy"))
}

fn copy_tree(source: &Path, target: &Path) -> io::Result<()> {
    fs::create_dir(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source_path)?;
        if metadata.file_type().is_symlink() {
            unix_fs::symlink(fs::read_link(&source_path)?, target_path)?;
        } else if metadata.is_dir() {
            copy_tree(&source_path, &target_path)?;
        } else if metadata.is_file() {
            fs::copy(&source_path, &target_path)?;
            fs::set_permissions(&target_path, metadata.permissions())?;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "unsupported legacy app-data entry {}",
                    source_path.display()
                ),
            ));
        }
    }
    fs::set_permissions(target, fs::metadata(source)?.permissions())?;
    Ok(())
}

/// Add the complete non-render Studio runtime plugin stack.
///
/// The real binary and the headless startup regression share this function so
/// Bevy validates identical cross-plugin set membership and ordering.
fn add_runtime_plugins(
    app: &mut App,
    theme_dir: Option<PathBuf>,
    noded_url: String,
    transport: Box<dyn MixerTransport>,
) {
    app.add_plugins((
        CtkThemePlugin::new(theme_dir),
        ChromePlugin,
        CtkWidgetsPlugin,
        FileRequesterPlugin,
        MenuBarPlugin,
        ActionBridgePlugin,
        action::ActionPlugin,
        file_io::FileIoPlugin,
        song_load::SongLoadPlugin,
        settings::SettingsPlugin,
        views::ViewsPlugin,
        MusicdMixerPlugin::with_transport(transport),
        app_port::StudioAppPortPlugin::new(noded_url),
    ));
}

#[derive(Resource, Clone, Copy)]
struct AutoplayEnabled(bool);

/// The transport footer's Play / Stop buttons, so the Space shortcut and the
/// song-load reset can drive the SAME activation path a pointer click takes.
#[derive(Resource, Clone, Copy)]
pub struct TransportButtons {
    pub play: Entity,
    pub stop: Entity,
}

fn parse_source(args: &[String]) -> Result<InProcessSource, String> {
    let multitone = args.iter().any(|arg| arg == "--multitone");
    let mut stems = None;
    let mut song = None;
    let mut soundfont = None;
    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        if let Some(slot) = match flag {
            "--stems" => Some(&mut stems),
            "--song" => Some(&mut song),
            "--soundfont" => Some(&mut soundfont),
            _ => None,
        } {
            let path = args
                .get(index + 1)
                .filter(|value| !value.starts_with("--"))
                .ok_or_else(|| format!("{flag} requires a path"))?;
            if slot.replace(PathBuf::from(path)).is_some() {
                return Err(format!("{flag} may only be supplied once"));
            }
            index += 2;
        } else {
            index += 1;
        }
    }
    let picked =
        usize::from(stems.is_some()) + usize::from(song.is_some()) + usize::from(multitone);
    if picked > 1 {
        return Err(
            "choose at most one of --stems <manifest.json>, --song <file.json|.oxm|.mid>, \
             or --multitone"
                .into(),
        );
    }
    if soundfont.is_some() && song.is_none() && picked > 0 {
        return Err("--soundfont only applies with --song (or a bare launch)".into());
    }
    if let Some(path) = stems {
        Ok(InProcessSource::StemManifest(path))
    } else if let Some(path) = song {
        Ok(InProcessSource::Song { path, soundfont })
    } else if multitone {
        Ok(InProcessSource::BenchmarkMultitone)
    } else {
        // Bare launch: an empty song session — File > Open Song / Open
        // SoundFont load everything through the native requester.
        Ok(InProcessSource::Empty { soundfont })
    }
}

/// Reactive rendering. Idle, the app redraws only on input (winit wakes the
/// loop) or a 200 ms heartbeat that drains Bus/async state — instead of a full
/// continuous render loop burning ~½ a core doing nothing. While the transport
/// plays it flips to `Continuous` so the playhead + meters animate at full
/// framerate, with an 0.8 s settle after stop so the meters can decay smoothly.
/// A 1 s startup grace keeps it continuous until the first UI is fully painted.
fn tune_update_mode(
    state: Res<MusicdMixerState>,
    time: Res<Time>,
    mut winit: ResMut<WinitSettings>,
    mut awake_until: Local<f64>,
) {
    let now = time.elapsed().as_secs_f64();
    if transport_is_playing(&state) {
        *awake_until = now + 0.8;
    }
    let want_continuous = now < 1.0 || now < *awake_until;
    let is_continuous = matches!(winit.focused_mode, UpdateMode::Continuous);
    if want_continuous != is_continuous {
        // Playback drives BOTH modes continuous — the playhead stays smooth even
        // when the window is unfocused (this app is often driven over Bus while
        // the operator watches an unfocused window). Idle drops focused to a
        // 200 ms reactive heartbeat and unfocused to 1 s low-power.
        if want_continuous {
            winit.focused_mode = UpdateMode::Continuous;
            winit.unfocused_mode = UpdateMode::Continuous;
        } else {
            winit.focused_mode = UpdateMode::reactive(std::time::Duration::from_millis(200));
            winit.unfocused_mode =
                UpdateMode::reactive_low_power(std::time::Duration::from_secs(1));
        }
    }
}

// Bevy systems expose each independently borrowed resource/query as a parameter.
#[allow(clippy::too_many_arguments)]
fn setup(
    mut commands: Commands,
    mut theme: ResMut<UiTheme>,
    mut theme_state: ResMut<ThemeState>,
    mut metrics: ResMut<CtkThemeMetrics>,
    smoke_flags: Res<SmokeFlags>,
    autoplay_enabled: Res<AutoplayEnabled>,
    editing: Option<Res<editor::SongEditor>>,
    asset_server: Res<AssetServer>,
    actions: Res<MenuActionRegistry>,
) {
    *theme = UiTheme(create_dark_theme());
    // The cosmix theme: built-in Ocean-dark ← shared
    // ~/.config/cosmix/theme.conf.mix (desktop-wide, same scheme key the web
    // uses) ← this app's own theme.conf.mix override. A scheme/mode change or
    // per-token hex from either file lands here.
    let app_cfg = ctk::app_dirs::AppDirs::resolve(IDENTITY.slug).map(|d| d.config());
    let spec = resolve_app_theme(app_cfg.as_deref());
    eprintln!("cosmix theme: {} {}", spec.scheme.name(), spec.mode.name());
    *metrics = spec.metrics.clone();
    apply_theme(&mut theme, &mut theme_state, &spec);
    commands.spawn(Camera2d);

    let style = StripStyle::compact();

    // 32 compact channel strips followed by the master. Channel 0's fader is
    // kept for the (opt-in) shared smoke harness.
    let mut smoke_fader: Option<Entity> = None;
    let mut strips: Vec<Entity> = (0..NUM_CHANNELS)
        .map(|channel| {
            let entities = spawn_channel_strip_styled(&mut commands, channel, &style);
            if channel == 0 {
                smoke_fader = Some(entities.fader);
            }
            entities.root
        })
        .collect();
    strips.push(spawn_master_strip(&mut commands, &style));
    if let Some(fader) = smoke_fader {
        if smoke_flags.write {
            commands.insert_resource(smoke::SmokeRun::new(fader));
        }
        if smoke_flags.stream {
            commands.insert_resource(smoke::StreamRun::new(fader));
        }
    }
    // The mixer view: the strips row, one of the three switchable views.
    let strips_row = commands
        .spawn((
            Node {
                flex_direction: FlexDirection::Row,
                column_gap: px(1),
                // Fill the window's spare height (matching the disp-skia
                // board): the row grows, Stretch pulls every strip to the
                // row's height, and each strip's fader row absorbs the slack.
                align_items: AlignItems::Stretch,
                flex_grow: 1.0,
                min_height: px(0),
                ..default()
            },
            ViewRoot(ActiveView::Mixer),
        ))
        .add_children(&strips)
        .id();

    // The waves view: per-track named lanes fill in lazily (offline render).
    let waves_view = commands
        .spawn((
            Node {
                position_type: PositionType::Relative,
                flex_direction: FlexDirection::Column,
                flex_grow: 1.0,
                min_height: px(0),
                display: Display::None,
                overflow: bevy::ui::Overflow::clip(),
                ..default()
            },
            bevy::feathers::theme::ThemeBackgroundColor(ctk::theme::tokens::SURFACE),
            ViewRoot(ActiveView::Waves),
            WavesContainer,
        ))
        .id();
    // The "rendering waves" overlay lives inside the container so it covers the
    // stale lanes; it survives arranger rebuilds (not a `WavesLane`).
    let spinner = views::spawn_waves_spinner(&mut commands);
    commands.entity(waves_view).add_child(spinner);

    // The piano-roll view (only on --song launches).
    let roll_view = editing.map(|_| {
        let roll = spawn_piano_roll(&mut commands, 240.0);
        // Re-shape the roll's root into a full-height switchable view.
        commands.entity(roll.root).insert((
            Node {
                width: percent(100),
                flex_grow: 1.0,
                min_height: px(0),
                display: Display::None,
                ..default()
            },
            ViewRoot(ActiveView::PianoRoll),
        ));
        roll.root
    });

    // The shared content area the three views swap inside.
    let mut view_children = vec![strips_row, waves_view];
    view_children.extend(roll_view);
    let content = commands
        .spawn((Node {
            width: percent(100),
            flex_grow: 1.0,
            min_height: px(0),
            align_items: AlignItems::Stretch,
            ..default()
        },))
        .add_children(&view_children)
        .id();

    let menu_defs = views::menu_defs();
    if let Err(issues) = validate_menu_against_registry(&menu_defs, actions.registry()) {
        panic!("Studio menu/action registry mismatch: {issues:?}");
    }
    let icons = IconSet::load(&asset_server);
    let menu_bar = spawn_menu_bar_with_icons(&mut commands, &menu_defs, &icons, &theme);
    commands.entity(menu_bar).insert(ActionBridgeBar);

    let footer = spawn_transport_footer(&mut commands, &style, SCRUBBER_WIDTH);
    commands.insert_resource(TransportButtons {
        play: footer.play,
        stop: footer.stop,
    });
    if autoplay_enabled.0 {
        commands.insert_resource(Autoplay { pressed: false });
    }
    let song = spawn_song_footer(&mut commands);
    // The persistent activity echo, pinned to the right of the (centered) song
    // footer. Absolute so it never shifts the centered title; spans the row
    // height to vertically centre its single line.
    let activity = commands
        .spawn(Node {
            position_type: PositionType::Absolute,
            right: px(12),
            top: px(0),
            bottom: px(0),
            align_items: AlignItems::Center,
            ..default()
        })
        .with_child((
            Text::new(""),
            TextFont::from_font_size(13.0),
            bevy::feathers::theme::ThemeTextColor(ctk::theme::tokens::TEXT_DIM),
            views::ActivityText,
        ))
        .id();
    let song_row = commands
        .spawn(Node {
            width: percent(100),
            position_type: PositionType::Relative,
            flex_direction: FlexDirection::Row,
            ..default()
        })
        .add_children(&[song, activity])
        .id();

    // Link status + the transient user-facing status message on one shared row.
    let status = spawn_status_bar(&mut commands, "Link: connecting");
    commands.entity(status.root).with_child((
        Text::new(""),
        TextFont::from_font_size(13.0),
        bevy::feathers::theme::ThemeTextColor(ctk::theme::tokens::TEXT_DIM),
        views::StatusMessageText,
    ));

    commands
        .spawn((
            Node {
                width: percent(100),
                // A hard height bound (not min-) so the strips row's
                // flex_grow has something real to fill against.
                height: percent(100),
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Start,
                row_gap: px(4),
                // No outer padding — the app fills the window to the edges.
                ..default()
            },
            bevy::feathers::theme::ThemeBackgroundColor(ctk::theme::tokens::SURFACE),
        ))
        .add_children(&[menu_bar, status.root, content, footer.root, song_row]);
}

#[cfg(test)]
mod startup_tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "cosmix-studio-{name}-{}-{nonce}",
            std::process::id()
        ))
    }

    #[test]
    fn identity_matches_package_and_workspace_path() {
        assert!(IDENTITY.validate().is_ok());
        assert_eq!(env!("CARGO_PKG_NAME"), format!("cosmix-{}", IDENTITY.slug));
        assert!(env!("CARGO_MANIFEST_DIR").ends_with(&format!("/apps/{}", IDENTITY.slug)));
    }

    #[test]
    fn legacy_app_root_moves_once_when_studio_root_is_absent() {
        let parent = temp_root("migration");
        let legacy = parent.join("midiseq");
        let studio = parent.join("studio");
        fs::create_dir_all(legacy.join("config")).unwrap();
        fs::write(legacy.join("config/keymap.conf.mix"), b"bindings: []\n").unwrap();

        assert_eq!(
            migrate_legacy_root(&studio, &legacy).unwrap(),
            LegacyMigration::Migrated("rename")
        );
        assert!(!legacy.exists());
        assert!(legacy_migration_marker(&studio).exists());
        assert_eq!(
            fs::read(studio.join("config/keymap.conf.mix")).unwrap(),
            b"bindings: []\n"
        );
        assert_eq!(
            migrate_legacy_root(&studio, &legacy).unwrap(),
            LegacyMigration::AlreadyHandled
        );
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn fresh_studio_root_gets_migration_marker() {
        let parent = temp_root("fresh-marker");
        let legacy = parent.join("midiseq");
        let studio = parent.join("studio");

        assert_eq!(
            migrate_legacy_root(&studio, &legacy).unwrap(),
            LegacyMigration::MarkedCurrentRoot
        );
        assert!(legacy_migration_marker(&studio).exists());
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn both_roots_warn_without_merging_or_marking() {
        let parent = temp_root("migration-conflict");
        let legacy = parent.join("midiseq");
        let studio = parent.join("studio");
        fs::create_dir_all(&legacy).unwrap();
        fs::create_dir_all(&studio).unwrap();

        assert_eq!(
            migrate_legacy_root(&studio, &legacy).unwrap(),
            LegacyMigration::Conflict
        );
        let warning = legacy_migration_warning(&studio, &legacy);
        assert!(warning.contains(&legacy.display().to_string()));
        assert!(warning.contains(&studio.display().to_string()));
        assert!(warning.contains("Back up both roots"));
        assert!(!legacy_migration_marker(&studio).exists());
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn migration_marker_suppresses_both_roots_warning() {
        let parent = temp_root("migration-marker-suppression");
        let legacy = parent.join("midiseq");
        let studio = parent.join("studio");
        fs::create_dir_all(&legacy).unwrap();
        fs::create_dir_all(&studio).unwrap();
        write_legacy_migration_marker(&studio).unwrap();

        assert_eq!(
            migrate_legacy_root(&studio, &legacy).unwrap(),
            LegacyMigration::AlreadyHandled
        );
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn fallback_copy_preserves_nested_files_and_symlinks() {
        let parent = temp_root("copy-tree");
        let source = parent.join("source");
        let target = parent.join("target");
        fs::create_dir_all(source.join("state")).unwrap();
        fs::write(source.join("state/song.json"), b"{}\n").unwrap();
        unix_fs::symlink("song.json", source.join("state/current")).unwrap();

        copy_tree(&source, &target).unwrap();
        assert_eq!(fs::read(target.join("state/song.json")).unwrap(), b"{}\n");
        assert_eq!(
            fs::read_link(target.join("state/current")).unwrap(),
            PathBuf::from("song.json")
        );
        fs::remove_dir_all(parent).unwrap();
    }

    struct HeadlessTransport;

    impl MixerTransport for HeadlessTransport {
        fn service_name(&self) -> &str {
            "studio-schedule-test"
        }

        fn issue_write(
            &mut self,
            _request_id: u64,
            _request: &cosmix_mixer_schema::WriteRequest,
        ) -> Result<(), String> {
            Ok(())
        }

        fn request_snapshot(&mut self, _request_id: u64) -> Result<(), String> {
            Ok(())
        }

        fn request_position(&mut self, _request_id: u64) -> Result<(), String> {
            Ok(())
        }

        fn poll_events(&mut self, out: &mut Vec<TransportEvent>) {
            out.clear();
        }

        fn poll_messages(&mut self, out: &mut Vec<TransportMessage>) {
            out.clear();
        }

        fn discard_backlog(&mut self) {}
    }

    #[test]
    fn real_runtime_plugin_stack_initialises_headless() {
        let mut app = App::new();
        app.set_error_handler(bevy::ecs::error::ignore)
            .add_plugins(MinimalPlugins);
        add_runtime_plugins(
            &mut app,
            None,
            "ws://127.0.0.1:1/studio-schedule-test".to_owned(),
            Box::new(HeadlessTransport),
        );
        app.finish();
        app.cleanup();

        app.update();
    }
}
