//! Opt-in composed-frame capture for unattended compositor testing.

use std::{
    env,
    error::Error,
    ffi::OsString,
    fmt,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use bevy::{
    camera::RenderTarget,
    prelude::*,
    render::view::screenshot::{Screenshot, ScreenshotCaptured},
    tasks::AsyncComputeTaskPool,
    window::PrimaryWindow,
};

const CAPTURE_DIR_ENV: &str = "COSMIX_CAPTURE_DIR";
const CAPTURE_EVERY_ENV: &str = "COSMIX_CAPTURE_EVERY";
const CAPTURE_ON_SIGNAL_ENV: &str = "COSMIX_CAPTURE_ON_SIGNAL";
const DEFAULT_CAPTURE_EVERY: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub(crate) struct FrameCaptureError(String);

impl fmt::Display for FrameCaptureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for FrameCaptureError {}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FrameCaptureConfig {
    directory: PathBuf,
    every: Duration,
    on_signal: bool,
}

fn parse_capture_config(
    directory: Option<OsString>,
    every: Option<OsString>,
    on_signal: bool,
) -> Result<Option<FrameCaptureConfig>, FrameCaptureError> {
    let Some(directory) = directory else {
        if every.is_some() || on_signal {
            return Err(FrameCaptureError(format!(
                "{CAPTURE_EVERY_ENV} and {CAPTURE_ON_SIGNAL_ENV} require {CAPTURE_DIR_ENV}"
            )));
        }
        return Ok(None);
    };
    let directory = PathBuf::from(directory);
    if directory.as_os_str().is_empty() {
        return Err(FrameCaptureError(format!(
            "{CAPTURE_DIR_ENV} must not be empty"
        )));
    }

    let every = match every {
        None => DEFAULT_CAPTURE_EVERY,
        Some(value) => {
            let value = value.into_string().map_err(|_| {
                FrameCaptureError(format!("{CAPTURE_EVERY_ENV} must be valid UTF-8"))
            })?;
            let seconds = value.parse::<u64>().map_err(|_| {
                FrameCaptureError(format!(
                    "{CAPTURE_EVERY_ENV} must be an integer number of seconds"
                ))
            })?;
            if seconds == 0 {
                return Err(FrameCaptureError(format!(
                    "{CAPTURE_EVERY_ENV} must be at least 1"
                )));
            }
            Duration::from_secs(seconds)
        }
    };

    Ok(Some(FrameCaptureConfig {
        directory,
        every,
        on_signal,
    }))
}

struct SignalRegistration(signal_hook::SigId);

impl Drop for SignalRegistration {
    fn drop(&mut self) {
        signal_hook::low_level::unregister(self.0);
    }
}

#[derive(Clone)]
struct CaptureSignal {
    requested: Arc<AtomicBool>,
    _registration: Arc<SignalRegistration>,
}

impl CaptureSignal {
    fn register() -> Result<Self, FrameCaptureError> {
        let requested = Arc::new(AtomicBool::new(false));
        let registration = signal_hook::flag::register(
            signal_hook::consts::signal::SIGUSR1,
            Arc::clone(&requested),
        )
        .map_err(|error| {
            FrameCaptureError(format!("could not register frame-capture SIGUSR1: {error}"))
        })?;
        Ok(Self {
            requested,
            _registration: Arc::new(SignalRegistration(registration)),
        })
    }

    fn take_request(&self) -> bool {
        self.requested.swap(false, Ordering::AcqRel)
    }
}

#[derive(Debug)]
struct CaptureCadence {
    every: Duration,
    last_periodic: Option<Instant>,
    pending_signal: bool,
}

impl CaptureCadence {
    fn new(every: Duration) -> Self {
        Self {
            every,
            last_periodic: None,
            pending_signal: false,
        }
    }

    fn should_capture(
        &mut self,
        now: Instant,
        signal_requested: bool,
        target_available: bool,
        capture_in_flight: bool,
    ) -> bool {
        self.pending_signal |= signal_requested;
        let periodic_due = self
            .last_periodic
            .is_none_or(|last| now.saturating_duration_since(last) >= self.every);
        if !target_available || capture_in_flight {
            return false;
        }
        if periodic_due {
            self.last_periodic = Some(now);
            self.pending_signal = false;
            return true;
        }
        if self.pending_signal {
            self.pending_signal = false;
            return true;
        }
        false
    }
}

#[derive(Component, Clone, Debug, Eq, PartialEq)]
pub(crate) struct FrameCaptureTarget {
    name: String,
}

impl FrameCaptureTarget {
    pub(crate) fn new(name: &str) -> Self {
        Self {
            name: sanitise_target_name(name),
        }
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }
}

#[derive(Component)]
struct PendingFrameCapture {
    final_path: PathBuf,
    temporary_path: PathBuf,
}

#[derive(Resource)]
struct FrameCaptureRuntime {
    directory: PathBuf,
    cadence: CaptureCadence,
    next_sequence: u64,
    signal: Option<CaptureSignal>,
}

impl FrameCaptureRuntime {
    fn allocate_paths(&mut self, target_name: &str) -> (PathBuf, PathBuf) {
        loop {
            let sequence = self.next_sequence;
            self.next_sequence = self.next_sequence.saturating_add(1);
            let filename = format!("frame-{target_name}-{sequence:06}.png");
            let final_path = self.directory.join(filename);
            let temporary_path = self.directory.join(format!(
                ".frame-{target_name}-{sequence:06}.tmp-{}.png",
                std::process::id()
            ));
            if !final_path.exists() && !temporary_path.exists() {
                return (final_path, temporary_path);
            }
        }
    }
}

pub(crate) struct FrameCapturePlugin {
    config: FrameCaptureConfig,
    signal: Option<CaptureSignal>,
}

impl FrameCapturePlugin {
    fn from_environment() -> Result<Option<Self>, FrameCaptureError> {
        let Some(config) = parse_capture_config(
            env::var_os(CAPTURE_DIR_ENV),
            env::var_os(CAPTURE_EVERY_ENV),
            env::var_os(CAPTURE_ON_SIGNAL_ENV).is_some(),
        )?
        else {
            return Ok(None);
        };
        fs::create_dir_all(&config.directory).map_err(|error| {
            FrameCaptureError(format!(
                "could not create frame-capture directory {}: {error}",
                config.directory.display()
            ))
        })?;
        if !config.directory.is_dir() {
            return Err(FrameCaptureError(format!(
                "frame-capture path is not a directory: {}",
                config.directory.display()
            )));
        }
        let signal = config.on_signal.then(CaptureSignal::register).transpose()?;
        Ok(Some(Self { config, signal }))
    }
}

impl Plugin for FrameCapturePlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(FrameCaptureRuntime {
            directory: self.config.directory.clone(),
            cadence: CaptureCadence::new(self.config.every),
            next_sequence: 1,
            signal: self.signal.clone(),
        })
        .add_systems(Update, request_frame_capture);
        info!(
            directory = %self.config.directory.display(),
            every_seconds = self.config.every.as_secs(),
            on_signal = self.config.on_signal,
            "composed-frame capture enabled"
        );
    }
}

pub(crate) fn install_from_environment(app: &mut App) -> Result<(), FrameCaptureError> {
    if let Some(plugin) = FrameCapturePlugin::from_environment()? {
        app.add_plugins(plugin);
    }
    Ok(())
}

fn request_frame_capture(
    mut commands: Commands,
    mut runtime: ResMut<FrameCaptureRuntime>,
    primary_window: Query<(), With<PrimaryWindow>>,
    kms_targets: Query<(&RenderTarget, &FrameCaptureTarget)>,
    captures: Query<(), With<Screenshot>>,
) {
    let kms_target_available = kms_targets
        .iter()
        .any(|(target, _)| matches!(target, RenderTarget::TextureView(_)));
    let target_available = !primary_window.is_empty() || kms_target_available;
    let signal_requested = runtime
        .signal
        .as_ref()
        .is_some_and(CaptureSignal::take_request);
    if !runtime.cadence.should_capture(
        Instant::now(),
        signal_requested,
        target_available,
        !captures.is_empty(),
    ) {
        return;
    }

    if !primary_window.is_empty() {
        spawn_capture(
            &mut commands,
            &mut runtime,
            "nested",
            Screenshot::primary_window(),
        );
        return;
    }
    for (target, capture_target) in &kms_targets {
        let RenderTarget::TextureView(handle) = target else {
            continue;
        };
        let name = capture_target.name().to_owned();
        spawn_capture(
            &mut commands,
            &mut runtime,
            &name,
            Screenshot::texture_view(*handle),
        );
    }
}

fn spawn_capture(
    commands: &mut Commands,
    runtime: &mut FrameCaptureRuntime,
    target_name: &str,
    screenshot: Screenshot,
) {
    let (final_path, temporary_path) = runtime.allocate_paths(target_name);
    commands
        .spawn((
            screenshot,
            PendingFrameCapture {
                final_path,
                temporary_path,
            },
        ))
        .observe(save_captured_frame);
}

fn save_captured_frame(captured: On<ScreenshotCaptured>, pending: Query<&PendingFrameCapture>) {
    let Ok(paths) = pending.get(captured.entity) else {
        error!(
            entity = %captured.entity,
            "captured frame has no destination paths"
        );
        return;
    };
    let image = captured.image.clone();
    let final_path = paths.final_path.clone();
    let temporary_path = paths.temporary_path.clone();
    AsyncComputeTaskPool::get()
        .spawn(async move {
            match write_png_atomic(image, &temporary_path, &final_path) {
                Ok(()) => info!(path = %final_path.display(), "composed frame captured"),
                Err(error) => error!(
                    path = %final_path.display(),
                    %error,
                    "could not save composed-frame capture"
                ),
            }
        })
        .detach();
}

fn write_png_atomic(
    image: bevy::image::Image,
    temporary_path: &Path,
    final_path: &Path,
) -> Result<(), FrameCaptureError> {
    let dynamic = image.try_into_dynamic().map_err(|error| {
        FrameCaptureError(format!(
            "captured texture format cannot be encoded: {error}"
        ))
    })?;
    let mut temporary = TemporaryCapture::create(temporary_path)?;
    dynamic
        .write_to(temporary.file_mut(), image::ImageFormat::Png)
        .map_err(|error| FrameCaptureError(format!("PNG encoding failed: {error}")))?;
    temporary
        .file_mut()
        .flush()
        .map_err(|error| FrameCaptureError(format!("PNG flush failed: {error}")))?;
    temporary.publish(final_path)
}

struct TemporaryCapture {
    path: PathBuf,
    file: Option<File>,
}

impl TemporaryCapture {
    fn create(path: &Path) -> Result<Self, FrameCaptureError> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|error| {
                FrameCaptureError(format!(
                    "could not create temporary capture {}: {error}",
                    path.display()
                ))
            })?;
        Ok(Self {
            path: path.to_owned(),
            file: Some(file),
        })
    }

    fn file_mut(&mut self) -> &mut File {
        self.file
            .as_mut()
            .expect("temporary capture file exists until publication")
    }

    fn publish(mut self, final_path: &Path) -> Result<(), FrameCaptureError> {
        drop(self.file.take());
        fs::rename(&self.path, final_path).map_err(|error| {
            FrameCaptureError(format!(
                "could not publish capture {}: {error}",
                final_path.display()
            ))
        })?;
        self.path.clear();
        Ok(())
    }
}

impl Drop for TemporaryCapture {
    fn drop(&mut self) {
        if !self.path.as_os_str().is_empty() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn sanitise_target_name(name: &str) -> String {
    let mut sanitised = String::with_capacity(name.len());
    let mut last_was_separator = false;
    for character in name.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
            sanitised.push(character);
            last_was_separator = false;
        } else if !last_was_separator && !sanitised.is_empty() {
            sanitised.push('-');
            last_was_separator = true;
        }
    }
    while sanitised.ends_with('-') {
        sanitised.pop();
    }
    if sanitised.is_empty() {
        "output".into()
    } else {
        sanitised
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use bevy::{
        asset::RenderAssetUsages,
        image::Image,
        render::render_resource::{Extent3d, TextureDimension, TextureFormat},
    };

    use super::*;

    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

    fn temporary_directory(name: &str) -> PathBuf {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let directory = env::temp_dir().join(format!(
            "cosmix-comp-frame-capture-{name}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&directory).expect("unique test directory is created");
        directory
    }

    #[test]
    fn capture_environment_defaults_to_one_second_and_accepts_signal_mode() {
        let config = parse_capture_config(Some("/tmp/captures".into()), None, true)
            .expect("capture environment is valid")
            .expect("capture is enabled");
        assert_eq!(config.directory, PathBuf::from("/tmp/captures"));
        assert_eq!(config.every, Duration::from_secs(1));
        assert!(config.on_signal);

        let configured =
            parse_capture_config(Some("/tmp/captures".into()), Some("7".into()), false)
                .expect("explicit cadence is valid")
                .expect("capture is enabled");
        assert_eq!(configured.every, Duration::from_secs(7));
    }

    #[test]
    fn capture_environment_rejects_zero_and_orphaned_companion_knobs() {
        assert!(
            parse_capture_config(Some("/tmp/captures".into()), Some("0".into()), false)
                .expect_err("zero cadence is refused")
                .to_string()
                .contains("at least 1")
        );
        assert!(
            parse_capture_config(None, Some("2".into()), false)
                .expect_err("cadence without a directory is refused")
                .to_string()
                .contains(CAPTURE_DIR_ENV)
        );
        assert!(
            parse_capture_config(None, None, true)
                .expect_err("signal mode without a directory is refused")
                .to_string()
                .contains(CAPTURE_DIR_ENV)
        );
        assert_eq!(
            parse_capture_config(None, None, false).expect("disabled capture is valid"),
            None
        );
    }

    #[test]
    fn cadence_is_immediate_and_retains_a_signal_until_capture_is_possible() {
        let start = Instant::now();
        let mut cadence = CaptureCadence::new(Duration::from_secs(10));
        assert!(!cadence.should_capture(start, true, false, false));
        assert!(cadence.pending_signal);
        assert!(cadence.should_capture(start, false, true, false));
        assert!(!cadence.pending_signal);
        assert!(!cadence.should_capture(start + Duration::from_secs(1), false, true, false));
        assert!(!cadence.should_capture(start + Duration::from_secs(2), true, true, true));
        assert!(cadence.should_capture(start + Duration::from_secs(2), false, true, false));
        assert!(cadence.should_capture(start + Duration::from_secs(10), false, true, false));
    }

    #[test]
    fn target_names_are_sanitised_and_existing_frames_are_not_reused() {
        assert_eq!(sanitise_target_name("DP-1"), "DP-1");
        assert_eq!(sanitise_target_name(" card/one ? "), "card-one");
        assert_eq!(sanitise_target_name("///"), "output");

        let directory = temporary_directory("filenames");
        let occupied = directory.join("frame-DP-1-000001.png");
        File::create(&occupied).expect("occupied capture path is created");
        let mut runtime = FrameCaptureRuntime {
            directory: directory.clone(),
            cadence: CaptureCadence::new(Duration::from_secs(1)),
            next_sequence: 1,
            signal: None,
        };
        let (first, first_temporary) = runtime.allocate_paths("DP-1");
        let (second, second_temporary) = runtime.allocate_paths("DP-1");
        assert_eq!(first.file_name().unwrap(), "frame-DP-1-000002.png");
        assert_eq!(second.file_name().unwrap(), "frame-DP-1-000003.png");
        assert_ne!(first, second);
        assert_ne!(first_temporary, second_temporary);
        fs::remove_dir_all(directory).expect("filename test directory is removed");
    }

    #[test]
    fn atomic_png_writer_preserves_bgra_capture_colour() {
        let directory = temporary_directory("png");
        let temporary = directory.join(".frame-test-000001.tmp.png");
        let final_path = directory.join("frame-test-000001.png");
        let image = Image::new(
            Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            vec![0x33, 0x22, 0x11, 0xff],
            TextureFormat::Bgra8UnormSrgb,
            RenderAssetUsages::MAIN_WORLD,
        );

        write_png_atomic(image, &temporary, &final_path).expect("capture is encoded");
        assert!(!temporary.exists(), "temporary path is atomically removed");
        let decoded = image::open(&final_path)
            .expect("published PNG can be decoded")
            .into_rgba8();
        assert_eq!(decoded.get_pixel(0, 0).0, [0x11, 0x22, 0x33, 0xff]);
        fs::remove_dir_all(directory).expect("PNG test directory is removed");
    }

    #[test]
    fn offscreen_scanout_capture_round_trip_preserves_content_checksum() {
        fn checksum(bytes: &[u8]) -> u64 {
            bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
                (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
            })
        }

        let directory = temporary_directory("scanout-checksum");
        let temporary = directory.join(".frame-scanout-HDMI-A-1-000001.tmp.png");
        let final_path = directory.join("frame-scanout-HDMI-A-1-000001.png");
        let image = Image::new(
            Extent3d {
                width: 2,
                height: 1,
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            vec![0x33, 0x22, 0x11, 0xff, 0x66, 0x55, 0x44, 0xff],
            TextureFormat::Bgra8Unorm,
            RenderAssetUsages::MAIN_WORLD,
        );

        write_png_atomic(image, &temporary, &final_path).expect("offscreen capture is encoded");
        let decoded = image::open(&final_path)
            .expect("offscreen capture can be read back")
            .into_rgba8();
        assert_eq!(
            checksum(decoded.as_raw()),
            checksum(&[0x11, 0x22, 0x33, 0xff, 0x44, 0x55, 0x66, 0xff])
        );
        fs::remove_dir_all(directory).expect("checksum test directory is removed");
    }
}
