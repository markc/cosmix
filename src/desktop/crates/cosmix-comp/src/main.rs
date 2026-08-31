//! Phase 1 of the CosMix compositor: daily-drivable nested Wayland clients.

mod backend;
mod bindings;
mod capture;
mod chrome_frame_material;
mod client_surface_material;
mod compositor_scene;
mod decoration;
mod decoration_scene;
#[cfg(feature = "frame-capture")]
mod frame_capture;
#[cfg(feature = "bus")]
mod port;
mod protocol;
mod shadow_material;

use std::{env, error::Error, ffi::OsString, io, mem, process::ExitCode, time::Duration};

use backend::{BackendKind, render::KmsRenderTargetPlugin};
use bevy::{
    app::AppExit,
    ecs::schedule::ScheduleLabel,
    input::{
        ButtonState as BevyButtonState,
        keyboard::{KeyCode, NativeKeyCode},
        mouse::{MouseButton, MouseButtonInput, MouseScrollUnit, MouseWheel},
        touch::TouchPhase,
    },
    prelude::*,
    render::{
        Render, RenderApp, RenderScheduleOrder, RenderSystems,
        pipelined_rendering::{PipelinedRenderingPlugin, RenderExtractApp},
        render_resource::PollType,
        renderer::RenderDevice,
        view::ExtractedWindows,
    },
    window::{
        CursorMoved, PresentMode, WindowBackendScaleFactorChanged, WindowEvent, WindowPlugin,
        WindowResized,
    },
};
use cosmix_deco::ChromeStyle;
use cosmix_wgpu_dmabuf::{
    DmabufImportPlugin, ImportedDmabufImages, ManualVulkanRenderer, WaitForSubmittedWork,
};
use smithay::backend::input::{AxisRelativeDirection, AxisSource};

use compositor_scene::{
    CompositorSceneFailed, CompositorScenePlugin, CompositorSceneSet, NestedSecurityPresentation,
    SceneCursorMode,
};
use decoration::DecorationStartup;
use decoration_scene::init_chrome_font_cx;
use protocol::{
    CaptureCompletionReporter, EcsAction, ExplicitSyncExposureMode, ExplicitSyncPreparation,
    ExplicitSyncStartupReport, ExplicitSyncStartupVerdict, HostAxis, HostButtonState, HostInput,
    SecurityPresentationReporter, WaylandRuntime, WaylandRuntimePolicy,
    judge_explicit_sync_startup,
};

const DEFAULT_SOCKET: &str = "cosmix-0";
const WINDOW_TITLE: &str = "CosMix Compositor";
const INITIAL_WIDTH: u32 = 960;
const INITIAL_HEIGHT: u32 = 640;

/// Latch terminal DMA-BUF teardown before either Bevy world can drop a use.
///
/// `App::run` moves the real App into its runner, so a guard in `run`'s stack
/// frame would run too late: winit destroys that App before returning. Keeping
/// the guard in the main world makes it unwind with the runner-owned App. Its
/// registry clone also prevents the retained callbacks from dropping before
/// this `Drop` runs; Bevy then drops render sub-apps after the main world.
#[derive(Resource)]
struct NestedDmabufTeardownGuard(ImportedDmabufImages);

impl NestedDmabufTeardownGuard {
    fn install(app: &mut App) {
        let imports = app.world().resource::<ImportedDmabufImages>().clone();
        app.insert_resource(Self(imports));
    }
}

impl Drop for NestedDmabufTeardownGuard {
    fn drop(&mut self) {
        self.0.begin_terminal_teardown();
    }
}

/// Say, once, what the protocol thread decided about explicit sync.
///
/// The thread logs its own decision as it makes it. This is the same fact where
/// the operator is already looking — beside the "starting nested CosMix
/// compositor" line — and, unlike that log, it is the *judged* outcome rather
/// than the raw one, so an absent global is not something the reader has to
/// reconstruct from two lines and the exposure mode.
///
/// Every message here is in the past tense on purpose. The protocol thread
/// replies on the readiness channel *before* it begins serving, so by the time
/// this runs the thread may already have withdrawn the global after a permanent
/// fault. A line claiming the global is live would then be describing a world
/// that had already ended; a line saying it went live at startup stays true.
fn report_explicit_sync_startup(report: Option<&ExplicitSyncStartupReport>) {
    let Some(report) = report else {
        // Only a runtime with no protocol thread, which production never builds.
        tracing::warn!("no protocol thread reported an explicit-sync startup outcome");
        return;
    };
    let detail = match &report.preparation {
        ExplicitSyncPreparation::SkippedByPolicy => "preparation skipped by exposure mode".into(),
        ExplicitSyncPreparation::Prepared(identity) => {
            format!("prepared {}", identity.resolved_path.display())
        }
        ExplicitSyncPreparation::Unavailable(reason) => format!("unavailable: {reason:?}"),
    };
    match judge_explicit_sync_startup(report) {
        ExplicitSyncStartupVerdict::DisabledAsConfigured => tracing::info!(
            detail,
            "explicit sync withheld by configuration; no linux-drm-syncobj-v1 global was \
             advertised"
        ),
        ExplicitSyncStartupVerdict::Advertised => tracing::info!(
            detail,
            "explicit sync started; linux-drm-syncobj-v1 was advertised at startup"
        ),
        ExplicitSyncStartupVerdict::Degraded => tracing::warn!(
            detail,
            "explicit sync was asked for and could not be prepared; clients fall back to \
             implicit sync"
        ),
        ExplicitSyncStartupVerdict::Inconsistent => tracing::error!(
            detail,
            global_advertised = report.global_advertised,
            "explicit-sync startup report contradicts itself; the global no longer follows from \
             the preparation outcome"
        ),
    }
}

/// Install the subscriber for the standalone live path.
///
/// The nested compositor gets its subscriber from Bevy's `LogPlugin`, but the
/// `kms-live` branch constructs its headless `App` with `LogPlugin` disabled.
/// Initialising only from that branch keeps Bevy's ownership of nested logging
/// intact while making every live-thread diagnostic observable. A malformed filter falls back to the
/// documented default instead of making a controlling-TTY recovery depend on
/// log syntax. Diagnostics go to stderr: stdout belongs to the VT (probe
/// reports, and the confirmation prompt when `--kms-confirm` is requested), and
/// the live procedure captures stderr —
/// the 2026-08-04 lockup run kept every event on the VT framebuffer, where the
/// hard reset destroyed them.
fn init_kms_live_tracing() -> Result<(), Box<dyn Error + Send + Sync>> {
    use bevy::log::tracing_subscriber::{EnvFilter, fmt};

    let filter = match env::var("RUST_LOG") {
        Ok(value) => EnvFilter::try_new(value).unwrap_or_else(|error| {
            eprintln!("invalid RUST_LOG filter ({error}); using info");
            EnvFilter::new("info")
        }),
        Err(env::VarError::NotPresent) => EnvFilter::new("info"),
        Err(error @ env::VarError::NotUnicode(_)) => {
            eprintln!("invalid RUST_LOG filter ({error}); using info");
            EnvFilter::new("info")
        }
    };
    fmt()
        .with_env_filter(filter)
        .with_writer(io::stderr)
        .with_thread_names(true)
        .with_target(true)
        .try_init()
}

fn main() -> ExitCode {
    let cli = match Cli::parse(env::args_os().skip(1)) {
        Ok(ParseOutcome::Run(cli)) => cli,
        Ok(ParseOutcome::ListBindings {
            keybindings_enabled,
            profile,
        }) => {
            print!(
                "{}",
                bindings::BindingState::for_profile(profile, keybindings_enabled).to_strict_data()
            );
            return ExitCode::SUCCESS;
        }
        Ok(ParseOutcome::KmsProbe) => {
            let report = backend::probe::run();
            print!("{}", report.to_strict_data());
            return if report.success() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            };
        }
        Ok(ParseOutcome::KmsWatch { seconds }) => {
            let report = backend::watch::run(Duration::from_secs(seconds));
            print!("{}", report.to_strict_data());
            return if report.success() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            };
        }
        Ok(ParseOutcome::KmsLive { argv, bus_service }) => {
            if let Err(error) = init_kms_live_tracing() {
                eprintln!("kms-live tracing initialisation failed: {error}");
                return ExitCode::FAILURE;
            }
            match backend::kms_live::authorise(&argv) {
                Ok(grant) => match backend::kms_live::execute_live(
                    grant,
                    bus_service.unwrap_or_else(|| "comp".into()),
                ) {
                    Ok(()) => {
                        return backend::kms_live::latched_signal_exit_code()
                            .map(ExitCode::from)
                            .unwrap_or(ExitCode::SUCCESS);
                    }
                    Err(error) => {
                        eprintln!("{}: {error}", error.reason_code());
                        if let Some(code) = error.exit_code() {
                            return ExitCode::from(code);
                        }
                    }
                },
                Err(refusal) => {
                    eprintln!("{}: {refusal}", refusal.reason_code());
                }
            }
            return ExitCode::FAILURE;
        }
        Ok(ParseOutcome::Help) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("{error}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    match run(*cli) {
        Ok(AppExit::Success) => ExitCode::SUCCESS,
        // Bevy reports its non-panic failures through the returned AppExit —
        // the render world's device-lost/uncaptured-error policy
        // (AppExit::Error(1)), a winit event-loop failure (AppExit::error()),
        // and the Ctrl-C handler (AppExit::Error(130)); panics still unwind
        // separately. Flattening the returned AppExit to SUCCESS made a
        // nested DeviceLost indistinguishable from a host window close.
        Ok(AppExit::Error(code)) => {
            eprintln!("cosmix-comp: nested compositor exited with error code {code}");
            ExitCode::from(code.get())
        }
        Err(error) => {
            eprintln!("cosmix-comp: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<AppExit, Box<dyn Error>> {
    let decoration = cli.decoration.clone();
    if cli.backend == BackendKind::Kms {
        let probe = backend::probe::run();
        if !probe.ready_for_bringup() {
            return Err(io::Error::other(format!(
                "KMS bring-up prerequisites unavailable: {}",
                probe
                    .first_error()
                    .unwrap_or("one or more required KMS bring-up guards failed")
            ))
            .into());
        }
        let _runtime = WaylandRuntime::new(
            &cli.socket,
            BackendKind::Kms,
            (INITIAL_WIDTH, INITIAL_HEIGHT),
            None,
            None,
            None,
            WaylandRuntimePolicy {
                keybindings_enabled: cli.keybindings_enabled,
                explicit_sync_exposure_mode: ExplicitSyncExposureMode::Disabled,
                decoration,
            },
        )?;
        return Err(io::Error::other(
            "KMS protocol bootstrap crossed the Rung A live-bringup boundary unexpectedly",
        )
        .into());
    }

    let renderer = ManualVulkanRenderer::new()?;
    let dmabuf_capabilities = renderer.capabilities().clone();
    let dmabuf_validator = renderer.dmabuf_validator();
    let retirement_adapter: Box<dyn WaitForSubmittedWork> = Box::new(renderer.retirement_adapter());
    let mut app = App::new();
    init_chrome_font_cx(&mut app);
    // Cargo feature unification can re-enable Bevy's `multi_threaded` feature
    // through another workspace member (notably CTK's UI stack). Disable the
    // pipelined renderer in the runtime plugin group, independent of features.
    // cosmix-comp enables `multi_threaded` itself so this disable target is
    // present even in a narrow `cargo run -p cosmix-comp` build.
    let default_plugins = DefaultPlugins
        .build()
        .disable::<PipelinedRenderingPlugin>()
        .set(WindowPlugin {
            primary_window: Some(Window {
                title: WINDOW_TITLE.into(),
                resolution: (INITIAL_WIDTH, INITIAL_HEIGHT).into(),
                present_mode: PresentMode::AutoVsync,
                ..default()
            }),
            ..default()
        });
    app.add_plugins(renderer.install_into(default_plugins))
        .add_plugins((DmabufImportPlugin, KmsRenderTargetPlugin))
        .add_plugins(capture::CaptureServicePlugin);
    #[cfg(feature = "frame-capture")]
    frame_capture::install_from_environment(&mut app)?;
    if std::env::var_os("COSMIX_DMABUF_LOG_IMPORTS").is_some_and(|value| value == "1") {
        app.world()
            .resource::<ImportedDmabufImages>()
            .enable_import_logging();
        tracing::info!("nested DMA-BUF import logging enabled");
    }
    NestedDmabufTeardownGuard::install(&mut app);
    let pipelined_rendering = app.is_plugin_added::<PipelinedRenderingPlugin>()
        || app.get_sub_app(RenderExtractApp).is_some();
    tracing::debug!(
        pipelined_rendering,
        "checked nested compositor render scheduling"
    );
    if pipelined_rendering {
        return Err(io::Error::other(
            "PipelinedRenderingPlugin remained active after runtime disable",
        )
        .into());
    }

    let policy = WaylandRuntimePolicy {
        keybindings_enabled: cli.keybindings_enabled,
        explicit_sync_exposure_mode: ExplicitSyncExposureMode::Production,
        decoration: decoration.clone(),
    };
    #[cfg(feature = "bus")]
    let mut runtime = WaylandRuntime::new_production(
        &cli.socket,
        BackendKind::Winit,
        (INITIAL_WIDTH, INITIAL_HEIGHT),
        Some(dmabuf_capabilities.clone()),
        Some(Box::new(dmabuf_validator)),
        Some(retirement_adapter),
        policy,
        cli.bus_service.unwrap_or_else(|| "comp-nested".into()),
    )?;
    #[cfg(not(feature = "bus"))]
    let mut runtime = WaylandRuntime::new(
        &cli.socket,
        BackendKind::Winit,
        (INITIAL_WIDTH, INITIAL_HEIGHT),
        Some(dmabuf_capabilities.clone()),
        Some(Box::new(dmabuf_validator)),
        Some(retirement_adapter),
        policy,
    )?;
    info!(
        socket = cli.socket,
        bridge = cosmix_wgpu_dmabuf::BRIDGE_STATUS,
        adapter = dmabuf_capabilities.adapter_name,
        dmabuf_formats = dmabuf_capabilities.formats.len(),
        "starting nested CosMix compositor"
    );
    report_explicit_sync_startup(runtime.explicit_sync_startup());
    let scene_feed = runtime.take_client_scene_feed()?;
    let capture_reporter = runtime.capture_completion_reporter();
    install_nested_security_presentation_completion(
        &mut app,
        runtime.security_presentation_reporter(),
        capture_reporter.clone(),
    );
    #[cfg(feature = "bus")]
    runtime.start_port().map_err(io::Error::other)?;

    // `App::run` reports how the app ended (window close / exit chord →
    // Success; render-error, device-lost, winit-loop, Ctrl-C → Error).
    // Propagate it so the process exit code stays truthful.
    let exit = app
        .insert_resource(runtime)
        .insert_resource(scene_feed)
        .insert_resource(capture_reporter)
        .insert_resource(decoration)
        .insert_resource(ClearColor(Color::srgb(0.025, 0.035, 0.065)))
        .init_resource::<HostInputQueue>()
        .add_plugins(CompositorScenePlugin::new(
            INITIAL_WIDTH,
            INITIAL_HEIGHT,
            SceneCursorMode::HostCursor,
        ))
        .add_systems(First, pump_wayland.after(CompositorSceneSet))
        .add_systems(Startup, setup_scene)
        .add_systems(Update, (animate_background, collect_host_input))
        .add_systems(Last, finish_wayland_frame)
        .run();
    Ok(exit)
}

const USAGE: &str = "\
Usage: cosmix-comp (--nested | --kms) [--socket <name>] [--bus-service <name>] [--no-keybindings] [--ssd | --no-ssd] [--chrome <mac|win11|cosmix>]
       cosmix-comp --list-bindings [--binding-profile <nested|kms-live>] [--no-keybindings]
       cosmix-comp --kms-probe
       cosmix-comp --kms-watch <seconds>
       cosmix-comp kms-live --device <path> --connector <name> [--bus-service <name>] [--presentation atomic] [--scale <decimal>] [--first-light] [--kms-confirm] [--ssd | --no-ssd] [--chrome <mac|win11|cosmix>]

Options:
  --nested           Run in a Bevy winit window inside the current desktop
  --kms              Select the bare-metal KMS backend (Rung A topology only)
  --socket <name>    Wayland socket name (default: cosmix-0)
  --bus-service      Override the Bus service name (default comp-nested/comp)
  --no-keybindings   Start with compositor key interception disabled
  --ssd              Explicitly enable server-side decorations (the default)
  --no-ssd           Disable server-side decorations
  --chrome <style>   Select mac, win11 or cosmix chrome (default mac; enables SSD)
                     Minimise restore is Super+Shift+M until a shell task switcher exists
  --list-bindings    Print the Mix strict-data binding table and exit
  --binding-profile  Select the binding profile to list (default: nested)
  --kms-probe        Run read-only Rung B KMS probes and print strict-data
  --kms-watch <secs> Watch read-only Rung C udev/DRM topology diffs
  kms-live           Authorise the sealed D-3 TTY live KMS path (non-default feature,
                     Cargo release profile, foreground VT; unattended by default)
  --scale <decimal>  Exact KMS output scale in 120ths (default 1.0; for example 2.5)
  --first-light      Diagnostic first-light scene instead of client content (kms-live only)
  --kms-confirm      Require the typed-nonce takeover confirmation (kms-live only; off by
                     default so an agent can drive the takeover with no human at the glass)
  --presentation     Select kms-live presentation (default atomic;
                     direct-display is accepted only to report its permanent retirement)
  -h, --help         Print this help
";

#[derive(Debug, PartialEq)]
struct Cli {
    backend: BackendKind,
    socket: String,
    keybindings_enabled: bool,
    decoration: DecorationStartup,
    bus_service: Option<String>,
}

enum ParseOutcome {
    Run(Box<Cli>),
    ListBindings {
        keybindings_enabled: bool,
        profile: bindings::BindingProfile,
    },
    KmsProbe,
    KmsWatch {
        seconds: u64,
    },
    KmsLive {
        argv: Vec<OsString>,
        bus_service: Option<String>,
    },
    Help,
}

impl Cli {
    fn parse(args: impl IntoIterator<Item = OsString>) -> Result<ParseOutcome, String> {
        let (args, bus_service) = extract_bus_service(args.into_iter().collect())?;
        if args.first().and_then(|argument| argument.to_str()) == Some("kms-live") {
            if args.len() == 2 && matches!(args[1].to_str(), Some("--help" | "-h")) {
                return Ok(ParseOutcome::Help);
            }
            return Ok(ParseOutcome::KmsLive {
                argv: args,
                bus_service,
            });
        }
        let original_args = args.clone();
        let mut args = args.into_iter();
        let mut backend = None;
        let mut socket = None;
        let mut no_keybindings = false;
        let mut list_bindings = false;
        let mut binding_profile = None;
        let mut kms_probe = false;
        let mut kms_watch = None;
        let mut ssd = None;
        let mut chrome = None;

        while let Some(argument) = args.next() {
            match argument.to_str() {
                Some("--nested") => {
                    if backend == Some(BackendKind::Kms) {
                        return Err("--nested and --kms are mutually exclusive".into());
                    }
                    backend = Some(BackendKind::Winit);
                }
                Some("--kms") => {
                    if backend == Some(BackendKind::Winit) {
                        return Err("--nested and --kms are mutually exclusive".into());
                    }
                    backend = Some(BackendKind::Kms);
                }
                Some("--no-keybindings") => {
                    if no_keybindings {
                        return Err("--no-keybindings may only be supplied once".into());
                    }
                    no_keybindings = true;
                }
                Some("--ssd") => {
                    if let Some(enabled) = ssd {
                        return Err(if enabled {
                            "--ssd may only be supplied once"
                        } else {
                            "--ssd and --no-ssd are mutually exclusive"
                        }
                        .into());
                    }
                    ssd = Some(true);
                }
                Some("--no-ssd") => {
                    if let Some(enabled) = ssd {
                        return Err(if enabled {
                            "--ssd and --no-ssd are mutually exclusive"
                        } else {
                            "--no-ssd may only be supplied once"
                        }
                        .into());
                    }
                    if chrome.is_some() {
                        return Err("--no-ssd cannot be combined with --chrome".into());
                    }
                    ssd = Some(false);
                }
                Some("--chrome") => {
                    if chrome.is_some() {
                        return Err("--chrome may only be supplied once".into());
                    }
                    let value = args
                        .next()
                        .ok_or_else(|| "--chrome requires a style".to_string())?;
                    let value = value
                        .to_str()
                        .ok_or_else(|| "chrome style must be valid UTF-8".to_string())?;
                    if value.starts_with("--") {
                        return Err("--chrome requires a style".into());
                    }
                    chrome = Some(
                        ChromeStyle::from_name(value)
                            .ok_or_else(|| format!("unknown chrome style: {value}"))?,
                    );
                    if ssd == Some(false) {
                        return Err("--no-ssd cannot be combined with --chrome".into());
                    }
                }
                Some("--list-bindings") => {
                    if list_bindings {
                        return Err("--list-bindings may only be supplied once".into());
                    }
                    list_bindings = true;
                }
                Some("--binding-profile") => {
                    if binding_profile.is_some() {
                        return Err("--binding-profile may only be supplied once".into());
                    }
                    let value = args
                        .next()
                        .ok_or_else(|| "--binding-profile requires a value".to_string())?;
                    binding_profile = Some(match value.to_str() {
                        Some("nested") => bindings::BindingProfile::Nested,
                        Some("kms-live") => bindings::BindingProfile::KmsLive,
                        Some(value) => {
                            return Err(format!("unknown binding profile: {value}"));
                        }
                        None => return Err("binding profile must be valid UTF-8".into()),
                    });
                }
                Some("--kms-probe") => {
                    if kms_probe {
                        return Err("--kms-probe may only be supplied once".into());
                    }
                    kms_probe = true;
                }
                Some("--kms-watch") => {
                    if kms_watch.is_some() {
                        return Err("--kms-watch may only be supplied once".into());
                    }
                    let value = args
                        .next()
                        .ok_or_else(|| "--kms-watch requires a duration in seconds".to_string())?;
                    let value = value
                        .to_str()
                        .ok_or_else(|| "watch duration must be valid UTF-8".to_string())?;
                    let seconds = value
                        .parse::<u64>()
                        .map_err(|_| "watch duration must be an integer number of seconds")?;
                    if !(1..=3_600).contains(&seconds) {
                        return Err("watch duration must be between 1 and 3600 seconds".into());
                    }
                    kms_watch = Some(seconds);
                }
                Some("--socket") => {
                    if socket.is_some() {
                        return Err("--socket may only be supplied once".into());
                    }
                    let value = args
                        .next()
                        .ok_or_else(|| "--socket requires a name".to_string())?;
                    let value = value
                        .into_string()
                        .map_err(|_| "socket name must be valid UTF-8".to_string())?;
                    if value.is_empty() || value.contains('/') {
                        return Err("socket name must be non-empty and contain no '/'".into());
                    }
                    socket = Some(value);
                }
                Some("kms-live") => {
                    return Ok(ParseOutcome::KmsLive {
                        argv: original_args,
                        bus_service,
                    });
                }
                Some("-h" | "--help") => return Ok(ParseOutcome::Help),
                Some(other) => return Err(format!("unknown argument: {other}")),
                None => return Err("arguments must be valid UTF-8".into()),
            }
        }

        let keybindings_enabled = !no_keybindings;
        if kms_probe {
            if backend.is_some()
                || socket.is_some()
                || no_keybindings
                || list_bindings
                || binding_profile.is_some()
                || kms_watch.is_some()
                || ssd.is_some()
                || chrome.is_some()
                || bus_service.is_some()
            {
                return Err("--kms-probe must be supplied by itself".into());
            }
            return Ok(ParseOutcome::KmsProbe);
        }
        if let Some(seconds) = kms_watch {
            if backend.is_some()
                || socket.is_some()
                || no_keybindings
                || list_bindings
                || binding_profile.is_some()
                || ssd.is_some()
                || chrome.is_some()
                || bus_service.is_some()
            {
                return Err("--kms-watch must be supplied by itself".into());
            }
            return Ok(ParseOutcome::KmsWatch { seconds });
        }
        if list_bindings {
            if bus_service.is_some() {
                return Err("--bus-service requires --nested, --kms, or kms-live".into());
            }
            return Ok(ParseOutcome::ListBindings {
                keybindings_enabled,
                profile: binding_profile.unwrap_or(bindings::BindingProfile::Nested),
            });
        }
        if binding_profile.is_some() {
            return Err("--binding-profile requires --list-bindings".into());
        }
        let backend = backend.ok_or_else(|| "one of --nested or --kms is required".to_string())?;

        Ok(ParseOutcome::Run(Box::new(Cli {
            backend,
            socket: socket.unwrap_or_else(|| DEFAULT_SOCKET.into()),
            keybindings_enabled,
            decoration: DecorationStartup::resolve(
                ssd.unwrap_or(true) || chrome.is_some(),
                chrome.unwrap_or(ChromeStyle::Mac),
            ),
            bus_service,
        })))
    }
}

fn extract_bus_service(args: Vec<OsString>) -> Result<(Vec<OsString>, Option<String>), String> {
    let mut retained = Vec::with_capacity(args.len());
    let mut service = None;
    let mut args = args.into_iter();
    while let Some(argument) = args.next() {
        if argument.to_str() != Some("--bus-service") {
            retained.push(argument);
            continue;
        }
        if service.is_some() {
            return Err("--bus-service may only be supplied once".into());
        }
        let value = args
            .next()
            .ok_or_else(|| "--bus-service requires a name".to_string())?
            .into_string()
            .map_err(|_| "Bus service name must be valid UTF-8".to_string())?;
        service = Some(accept_bus_service(value)?);
    }
    Ok((retained, service))
}

#[cfg(feature = "bus")]
fn accept_bus_service(value: String) -> Result<String, String> {
    crate::port::validate_service_name(&value)?;
    Ok(value)
}

#[cfg(not(feature = "bus"))]
fn accept_bus_service(_value: String) -> Result<String, String> {
    Err("--bus-service requires the compositor 'bus' feature".into())
}

#[derive(Component)]
struct BackgroundQuad;

fn setup_scene(mut commands: Commands) {
    commands.spawn((
        Camera2d,
        capture::CaptureOutputSource {
            source_id: backend::CaptureSourceId::Nested {
                output_name: "cosmix-nested-0".into(),
            },
            output_name: "cosmix-nested-0".into(),
        },
    ));
    commands.spawn((
        BackgroundQuad,
        Sprite::from_color(Color::hsl(195.0, 0.82, 0.34), Vec2::new(250.0, 170.0)),
        Transform::from_xyz(0.0, 0.0, 0.0),
    ));
}

fn animate_background(
    time: Res<Time>,
    mut quad: Single<(&mut Transform, &mut Sprite), With<BackgroundQuad>>,
    damage: Res<capture::OutputDamageJournal>,
) {
    let elapsed = time.elapsed_secs();
    quad.0.translation.x = elapsed.sin() * 180.0;
    quad.0.translation.y = (elapsed * 0.7).cos() * 80.0;
    quad.0.rotation = Quat::from_rotation_z(elapsed * 0.65);
    quad.1.color = Color::hsl((elapsed * 55.0) % 360.0, 0.72, 0.34);
    damage.mark_nested_base_full();
}

#[derive(Resource)]
struct HostInputQueue {
    pending: Vec<HostInput>,
    keyboard_focused: bool,
    scrolling_axes: ScrollingAxes,
}

impl Default for HostInputQueue {
    fn default() -> Self {
        Self {
            pending: Vec::new(),
            keyboard_focused: true,
            scrolling_axes: ScrollingAxes::default(),
        }
    }
}

/// Which scroll axes currently have an open gesture on the nested transport.
///
/// A lift ends only the axes that were actually scrolling. Without this, the
/// lift at the end of a purely vertical gesture would also claim the horizontal
/// axis stopped — a sequence the client was never told had started.
///
/// Only continuous input touches this. A wheel has no gesture and cannot end
/// one, so letting a detent mark an axis in flight would leave that mark set
/// forever, and the next trackpad lift would stop an axis whose motion came
/// from the wheel.
#[derive(Default)]
pub(crate) struct ScrollingAxes {
    horizontal: bool,
    vertical: bool,
}

/// One wheel detent in `wl_pointer.axis` units — the scale libinput defines and
/// `protocol::input` converts v120 with, so a client cannot tell the transports
/// apart.
const WHEEL_LINE_AMOUNT: f64 = 15.0;

/// Translate one Bevy wheel message into the seat's scroll command.
///
/// Bevy reports both components on every message, so a zero component is
/// ambiguous on its face. The resolution differs by device, so the two are
/// translated separately rather than through one parameterised path.
///
/// Returns `None` when the message carries no axis worth a frame.
pub(crate) fn host_axis_from_wheel(
    event: &MouseWheel,
    scrolling: &mut ScrollingAxes,
    time: u32,
) -> Option<HostInput> {
    let (horizontal, vertical, source) = match event.unit {
        MouseScrollUnit::Line => (
            wheel_detents(event.x),
            wheel_detents(event.y),
            AxisSource::Wheel,
        ),
        MouseScrollUnit::Pixel => {
            if matches!(event.phase, TouchPhase::Started) {
                // A new gesture inherits nothing from the last one. This is a
                // backstop, not the main defence: winit's Wayland pointer only
                // ever reaches `Started` from a phase that is neither `Started`
                // nor `Moved`, so a wheel click or a gesture the pointer left
                // mid-flight leaves the phase at `Moved` and the next gesture
                // never announces itself. Pointer leave is what really clears
                // this.
                *scrolling = ScrollingAxes::default();
            }
            // A cancellation is terminal, so its components are not motion:
            // re-arming an axis from a delta on a cancelled gesture would owe a
            // stop that nothing is left to send.
            let cancelled = matches!(event.phase, TouchPhase::Canceled);
            let ending = cancelled || matches!(event.phase, TouchPhase::Ended);
            let (x, y) = if cancelled {
                (0.0, 0.0)
            } else {
                (event.x, event.y)
            };
            (
                gesture_axis(x, ending, &mut scrolling.horizontal),
                gesture_axis(y, ending, &mut scrolling.vertical),
                // Deliberately not `Finger`. A high-resolution mouse wheel and a
                // trackpad are indistinguishable at the first message, so
                // committing to `Finger` would promise a lift a mouse never
                // sends; `wl_pointer.axis_stop` is legal for a continuous
                // source, so the lift reaches the client either way.
                AxisSource::Continuous,
            )
        }
    };
    if horizontal.is_none() && vertical.is_none() {
        return None;
    }
    Some(HostInput::PointerAxis {
        horizontal,
        vertical,
        source,
        // The nested backend cannot observe natural-scrolling configuration
        // through Bevy, so it reports the physical direction as matching.
        relative_direction: (
            AxisRelativeDirection::Identical,
            AxisRelativeDirection::Identical,
        ),
        time,
    })
}

/// Stop whatever the nested transport still has in flight, and forget it.
///
/// Called when the gesture's end will never be reported to us — the pointer
/// left — rather than when it is reported. Emits nothing when no axis was
/// scrolling, so the common case costs no frame.
pub(crate) fn end_scrolling_gesture(scrolling: &mut ScrollingAxes, time: u32) -> Option<HostInput> {
    let horizontal = gesture_axis(0.0, true, &mut scrolling.horizontal);
    let vertical = gesture_axis(0.0, true, &mut scrolling.vertical);
    if horizontal.is_none() && vertical.is_none() {
        return None;
    }
    Some(HostInput::PointerAxis {
        horizontal,
        vertical,
        source: AxisSource::Continuous,
        relative_direction: (
            AxisRelativeDirection::Identical,
            AxisRelativeDirection::Identical,
        ),
        time,
    })
}

/// One axis of a discrete wheel.
///
/// A zero is an axis this click did not turn, so it is absent rather than a
/// reported zero. There is no stop to report either way: winit resolves
/// discrete scrolling to `TouchPhase::Moved` unconditionally
/// (`winit-0.30.13/src/platform_impl/linux/wayland/seat/pointer/mod.rs:182-190`),
/// so a wheel never reaches the ending branch at all.
fn wheel_detents(value: f32) -> Option<HostAxis> {
    if value == 0.0 {
        return None;
    }
    Some(HostAxis {
        // Bevy reports positive values for up/left; Wayland reports positive
        // values for down/right.
        amount: -f64::from(value) * WHEEL_LINE_AMOUNT,
        v120: Some((-value * 120.0).round() as i32),
    })
}

/// One axis of a continuous gesture.
///
/// `ending` is a property of the *message*, not of this axis: winit raises
/// `TouchPhase::Ended` when **either** axis stops and still carries both deltas
/// (`wayland/seat/pointer/mod.rs:182-184`). So the phase alone cannot say which
/// axis ended — the value does. A non-zero component is an axis that is still
/// moving and must keep its motion even on an ending message; only a zero on an
/// axis that was actually scrolling is that axis's stop.
///
/// Reading `ending` as "both axes stopped" dropped the surviving axis's motion
/// and fabricated a stop for it in the same frame.
///
/// The value is not merely the best available signal, it is the one the
/// protocol nominates. `wayland.xml`'s `wl_pointer.frame` contract: "When a
/// wl_pointer.axis and a wl_pointer.axis_stop event occur within the same
/// frame, this indicates that axis movement in one axis has stopped but
/// continues in the other axis." The stop therefore belongs to the axis that
/// is *not* carrying motion, which is exactly what a zero component identifies.
///
/// A same-axis pairing — a final non-zero delta and that axis's own stop in one
/// frame — would be lost here, and cannot be recovered: SCTK merges the two
/// into one `AxisScroll` (`stop |= other.stop`, `absolute += other.absolute`,
/// smithay-client-toolkit `seat/pointer/mod.rs:73-77`) and winit then collapses
/// either axis's stop bit into the single `TouchPhase`, so nothing downstream
/// of winit can tell that shape from a live diagonal. Guessing the other way —
/// stopping every armed axis on an ending message — is not a fix but a trade
/// for a worse defect: it kills the axis the frame contract says is still
/// moving, which is the ordinary two-finger diagonal lift.
fn gesture_axis(value: f32, ending: bool, in_flight: &mut bool) -> Option<HostAxis> {
    if value != 0.0 {
        *in_flight = true;
        return Some(HostAxis {
            amount: -f64::from(value),
            v120: None,
        });
    }
    if !ending || !*in_flight {
        return None;
    }
    *in_flight = false;
    // A reported zero is the stop. `v120` stays absent because a detent count
    // of zero is not something a device reports at the end of a swipe.
    Some(HostAxis {
        amount: 0.0,
        v120: None,
    })
}

fn collect_host_input(
    mut cursor_events: MessageReader<CursorMoved>,
    mut button_events: MessageReader<MouseButtonInput>,
    mut window_events: MessageReader<WindowEvent>,
    mut resize_events: MessageReader<WindowResized>,
    mut scale_events: MessageReader<WindowBackendScaleFactorChanged>,
    mut queue: ResMut<HostInputQueue>,
) {
    // Bevy's input messages carry no device timestamp, so the collection point
    // is the earliest honest time available. Sampled once per run so a batch
    // reads as one instant rather than as events spread across the collection
    // itself.
    let time = protocol::monotonic_millis();

    for event in resize_events.read() {
        queue.pending.push(HostInput::OutputResized {
            width: event.width.max(1.0) as u32,
            height: event.height.max(1.0) as u32,
        });
    }

    for event in scale_events.read() {
        queue.pending.push(HostInput::OutputScaleChanged {
            scale: event.scale_factor,
        });
    }

    for event in cursor_events.read() {
        queue.pending.push(HostInput::PointerMotionAbsolute {
            x: f64::from(event.position.x),
            y: f64::from(event.position.y),
            time,
        });
    }

    for event in button_events.read() {
        if let Some(button) = linux_mouse_button(event.button) {
            queue.pending.push(HostInput::PointerButton {
                button,
                state: host_button_state(event.state),
                time,
            });
        }
    }

    // WindowEvent is the only Bevy surface that preserves OS ordering across
    // pointer, focus and keyboard events. Scroll is read from here rather than
    // from `MessageReader<MouseWheel>` for that reason and no other: Bevy
    // writes both, but only this stream keeps a gesture in order against the
    // pointer leave that terminates it, and the two can share one long
    // nested-compositor frame. Draining wheel messages first would let a leave
    // that preceded a scroll synthetically stop it.
    for event in window_events.read() {
        match event {
            WindowEvent::MouseWheel(event) => {
                if let Some(input) = host_axis_from_wheel(event, &mut queue.scrolling_axes, time) {
                    queue.pending.push(input);
                }
            }
            // Pointer leave is where the lift is lost, not focus loss. winit
            // resets its scroll phase on neither, but only leaving stops the
            // axis events arriving, and under Wayland the two are independent:
            // keyboard focus can move to another window while the pointer keeps
            // scrolling in ours, so ending the gesture on focus loss would
            // truncate a live scroll. Ending it here is the honest report —
            // from the client's side the gesture did end. Without it the stale
            // mark survives, the next gesture arrives as `Moved` and never
            // trips the `Started` reset, and its lift stops an axis that was
            // never scrolling.
            WindowEvent::CursorLeft(_) => {
                if let Some(stop) = end_scrolling_gesture(&mut queue.scrolling_axes, time) {
                    queue.pending.push(stop);
                }
            }
            WindowEvent::WindowFocused(event) => {
                queue.keyboard_focused = event.focused;
                if !event.focused {
                    queue.pending.push(HostInput::KeyboardFocusLost);
                }
            }
            WindowEvent::KeyboardInput(event) if queue.keyboard_focused => {
                if event.repeat {
                    continue;
                }
                if let Some(evdev_code) = evdev_keycode(event.key_code) {
                    queue.pending.push(HostInput::key_from_evdev(
                        evdev_code,
                        host_button_state(event.state),
                        time,
                    ));
                } else {
                    debug!(key = ?event.key_code, "host key has no evdev mapping");
                }
            }
            // Bevy appends synthetic releases after a sole focus loss. The
            // protocol reset already reconciled those keys authoritatively,
            // so forwarding the later synthetic copies would emit duplicate
            // releases. A focus gain re-arms real input in OS event order.
            WindowEvent::KeyboardInput(_) | WindowEvent::KeyboardFocusLost(_) => {}
            _ => {}
        }
    }
}

fn host_button_state(state: BevyButtonState) -> HostButtonState {
    match state {
        BevyButtonState::Pressed => HostButtonState::Pressed,
        BevyButtonState::Released => HostButtonState::Released,
    }
}

fn linux_mouse_button(button: MouseButton) -> Option<u32> {
    Some(match button {
        MouseButton::Left => 0x110,
        MouseButton::Right => 0x111,
        MouseButton::Middle => 0x112,
        MouseButton::Back => 0x113,
        MouseButton::Forward => 0x114,
        MouseButton::Other(number) => 0x115_u32.checked_add(u32::from(number))?,
    })
}

/// Translate Bevy's physical-key vocabulary to Linux evdev codes.
fn evdev_keycode(key: KeyCode) -> Option<u32> {
    use KeyCode::*;

    Some(match key {
        Unidentified(NativeKeyCode::Xkb(code)) => code.checked_sub(8)?,
        Escape => 1,
        Digit1 => 2,
        Digit2 => 3,
        Digit3 => 4,
        Digit4 => 5,
        Digit5 => 6,
        Digit6 => 7,
        Digit7 => 8,
        Digit8 => 9,
        Digit9 => 10,
        Digit0 => 11,
        Minus => 12,
        Equal => 13,
        Backspace => 14,
        Tab => 15,
        KeyQ => 16,
        KeyW => 17,
        KeyE => 18,
        KeyR => 19,
        KeyT => 20,
        KeyY => 21,
        KeyU => 22,
        KeyI => 23,
        KeyO => 24,
        KeyP => 25,
        BracketLeft => 26,
        BracketRight => 27,
        Enter => 28,
        ControlLeft => 29,
        KeyA => 30,
        KeyS => 31,
        KeyD => 32,
        KeyF => 33,
        KeyG => 34,
        KeyH => 35,
        KeyJ => 36,
        KeyK => 37,
        KeyL => 38,
        Semicolon => 39,
        Quote => 40,
        Backquote => 41,
        ShiftLeft => 42,
        Backslash => 43,
        KeyZ => 44,
        KeyX => 45,
        KeyC => 46,
        KeyV => 47,
        KeyB => 48,
        KeyN => 49,
        KeyM => 50,
        Comma => 51,
        Period => 52,
        Slash => 53,
        ShiftRight => 54,
        NumpadMultiply => 55,
        AltLeft => 56,
        Space => 57,
        CapsLock => 58,
        F1 => 59,
        F2 => 60,
        F3 => 61,
        F4 => 62,
        F5 => 63,
        F6 => 64,
        F7 => 65,
        F8 => 66,
        F9 => 67,
        F10 => 68,
        NumLock => 69,
        ScrollLock => 70,
        Numpad7 => 71,
        Numpad8 => 72,
        Numpad9 => 73,
        NumpadSubtract => 74,
        Numpad4 => 75,
        Numpad5 => 76,
        Numpad6 => 77,
        NumpadAdd => 78,
        Numpad1 => 79,
        Numpad2 => 80,
        Numpad3 => 81,
        Numpad0 => 82,
        NumpadDecimal => 83,
        IntlBackslash => 86,
        F11 => 87,
        F12 => 88,
        NumpadEnter => 96,
        ControlRight => 97,
        NumpadDivide => 98,
        PrintScreen => 99,
        AltRight => 100,
        Home => 102,
        ArrowUp => 103,
        PageUp => 104,
        ArrowLeft => 105,
        ArrowRight => 106,
        End => 107,
        ArrowDown => 108,
        PageDown => 109,
        Insert => 110,
        Delete => 111,
        AudioVolumeMute => 113,
        AudioVolumeDown => 114,
        AudioVolumeUp => 115,
        Power => 116,
        NumpadEqual => 117,
        Pause => 119,
        SuperLeft => 125,
        SuperRight => 126,
        ContextMenu => 127,
        _ => return None,
    })
}

fn pump_wayland(world: &mut World) {
    if world.contains_resource::<CompositorSceneFailed>() {
        return;
    }
    let commands = world
        .resource::<WaylandRuntime>()
        .drain_kms_render_commands()
        .unwrap_or_else(|error| panic!("KMS render command path failed: {error}"));
    if !commands.is_empty() {
        backend::render::send_render_commands(world, commands)
            .unwrap_or_else(|error| panic!("KMS render command delivery failed: {error:?}"));
    }

    if world.contains_resource::<backend::render::KmsRegistrarReplies>() {
        let replies = backend::render::drain_registrar_replies(world)
            .unwrap_or_else(|error| panic!("KMS registrar reply path failed: {error:?}"));
        for reply in replies {
            world
                .resource::<WaylandRuntime>()
                .submit_kms_render_reply(reply)
                .unwrap_or_else(|error| panic!("KMS render reply delivery failed: {error}"));
        }
    }

    let actions = world.resource::<WaylandRuntime>().drain_ecs_actions();
    match actions {
        Ok(actions) => {
            for action in actions {
                match action {
                    EcsAction::ExitNestedCompositor => {
                        world.write_message(AppExit::Success);
                    }
                }
            }
        }
        Err(error) => panic!("{error}"),
    }
}

#[derive(ScheduleLabel, Clone, Debug, Eq, Hash, PartialEq)]
struct NestedPostPresent;

#[derive(Resource, Default)]
struct NestedPresentCandidate {
    window: Option<Entity>,
    epochs: Vec<(u64, protocol::SecurityPresentationTarget)>,
    captures: Vec<capture::PendingCapturePresentation>,
}

#[derive(Resource)]
struct NestedPresentationCompletion {
    reporter: SecurityPresentationReporter,
    capture_reporter: CaptureCompletionReporter,
}

/// Install the production nested security barrier at Bevy's real completion
/// boundary: after queue submission and `SurfaceTexture::present()`.
fn install_nested_security_presentation_completion(
    app: &mut App,
    reporter: SecurityPresentationReporter,
    capture_reporter: CaptureCompletionReporter,
) {
    let pending = NestedSecurityPresentation::default();
    app.insert_resource(pending.clone());
    let capture_pending = app
        .world()
        .resource::<capture::CapturePresentationPending>()
        .clone();
    let render_app = app
        .get_sub_app_mut(RenderApp)
        .expect("nested renderer has a render sub-app");
    render_app
        .insert_resource(pending)
        .insert_resource(capture_pending)
        .insert_resource(NestedPresentationCompletion {
            reporter,
            capture_reporter,
        })
        .init_resource::<NestedPresentCandidate>()
        .init_schedule(NestedPostPresent)
        .add_systems(
            Render,
            capture_nested_present_candidate
                .in_set(RenderSystems::Render)
                .after(capture::CaptureRenderSet),
        )
        .add_systems(NestedPostPresent, complete_nested_security_presentation);
    render_app
        .world_mut()
        .resource_mut::<RenderScheduleOrder>()
        .insert_after(Render, NestedPostPresent);
}

fn capture_nested_present_candidate(
    pending: Res<NestedSecurityPresentation>,
    capture_pending: Res<capture::CapturePresentationPending>,
    windows: Res<ExtractedWindows>,
    mut candidate: ResMut<NestedPresentCandidate>,
) {
    candidate.window = None;
    candidate.epochs.clear();
    candidate.captures.clear();
    let Some(primary) = windows.primary else {
        return;
    };
    let Some(_window) = windows.get(&primary) else {
        return;
    };
    let epochs = pending.snapshot();
    let captures = capture_pending.take();
    if epochs.is_empty() && captures.is_empty() {
        return;
    }
    candidate.window = Some(primary);
    candidate.epochs = epochs;
    candidate.captures = captures;
}

fn complete_nested_security_presentation(
    pending: Res<NestedSecurityPresentation>,
    completion: Res<NestedPresentationCompletion>,
    capture_pending: Res<capture::CapturePresentationPending>,
    render_device: Res<RenderDevice>,
    windows: Res<ExtractedWindows>,
    mut candidate: ResMut<NestedPresentCandidate>,
) {
    let Some(window_id) = candidate.window.take() else {
        return;
    };
    let epochs = mem::take(&mut candidate.epochs);
    let captures = mem::take(&mut candidate.captures);
    // `present()` consumes the acquired swapchain texture. If it remains,
    // rendering or presentation was skipped and the epoch stays pending.
    if windows
        .get(&window_id)
        .is_none_or(|window| window.swap_chain_texture.is_some())
    {
        capture_pending.publish(captures);
        return;
    }
    if let Some((seconds, nanoseconds)) = monotonic_capture_timestamp() {
        for capture in captures {
            completion
                .capture_reporter
                .presented(capture::CapturePresented {
                    id: capture.id,
                    source_id: capture.source_id,
                    frame_token: capture.frame_token,
                    generation: capture.generation,
                    security_epoch: capture.security_epoch,
                    seconds,
                    nanoseconds,
                });
        }
    } else {
        for capture in captures {
            completion.capture_reporter.failed(
                capture.id,
                capture.generation,
                capture.security_epoch,
            );
        }
    }
    if let Err(error) = poll_nested_security_gpu(epochs.len(), || {
        render_device.poll(PollType::wait_indefinitely())
    }) {
        error!(%error, "nested security frame GPU completion failed");
        return;
    }

    let mut reported = Vec::new();
    for (epoch, presentation) in epochs {
        match completion
            .reporter
            .presented(epoch, presentation.output.clone())
        {
            Ok(()) => reported.push((epoch, presentation)),
            Err(error) => {
                error!(%error, "nested security presentation report failed");
                break;
            }
        }
    }
    pending.complete(&reported);
}

fn poll_nested_security_gpu<T, E>(
    epoch_count: usize,
    poll: impl FnOnce() -> Result<T, E>,
) -> Result<(), E> {
    if epoch_count == 0 {
        return Ok(());
    }
    poll().map(|_| ())
}

fn monotonic_capture_timestamp() -> Option<(u64, u32)> {
    let mut timestamp = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `timestamp` is valid writable storage for one timespec.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut timestamp) } == 0 {
        Some((
            u64::try_from(timestamp.tv_sec).ok()?,
            u32::try_from(timestamp.tv_nsec).ok()?,
        ))
    } else {
        None
    }
}

fn finish_wayland_frame(world: &mut World) {
    if world.contains_resource::<CompositorSceneFailed>() {
        return;
    }
    let inputs = mem::take(&mut world.resource_mut::<HostInputQueue>().pending);
    if let Err(error) = world.resource::<WaylandRuntime>().finish_frame(inputs) {
        panic!("{error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn parse(args: &[&str]) -> Result<ParseOutcome, String> {
        Cli::parse(args.iter().map(OsString::from))
    }

    #[test]
    fn capture_without_security_epoch_performs_no_blocking_poll() {
        let polls = AtomicUsize::new(0);
        let result = poll_nested_security_gpu(0, || {
            polls.fetch_add(1, Ordering::Relaxed);
            Ok::<(), ()>(())
        });
        assert_eq!(result, Ok(()));
        assert_eq!(polls.load(Ordering::Relaxed), 0);

        poll_nested_security_gpu(1, || {
            polls.fetch_add(1, Ordering::Relaxed);
            Ok::<(), ()>(())
        })
        .unwrap();
        assert_eq!(polls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn nested_defaults_to_cosmix_zero() {
        let ParseOutcome::Run(cli) = parse(&["--nested"]).expect("valid arguments") else {
            panic!("expected runnable CLI");
        };
        assert_eq!(
            *cli,
            Cli {
                backend: BackendKind::Winit,
                socket: DEFAULT_SOCKET.into(),
                keybindings_enabled: true,
                decoration: DecorationStartup::default(),
                bus_service: None,
            }
        );
    }

    #[test]
    fn nested_window_title_is_phase_neutral() {
        assert_eq!(WINDOW_TITLE, "CosMix Compositor");
    }

    #[test]
    fn socket_name_can_be_overridden() {
        let ParseOutcome::Run(cli) =
            parse(&["--nested", "--socket", "cosmix-test"]).expect("valid arguments")
        else {
            panic!("expected runnable CLI");
        };
        assert_eq!(cli.socket, "cosmix-test");
        assert!(cli.keybindings_enabled);
    }

    #[cfg(feature = "bus")]
    #[test]
    fn bus_service_override_is_validated_and_reaches_nested_and_kms_live() {
        let ParseOutcome::Run(cli) =
            parse(&["--nested", "--bus-service", "comp-test"]).expect("valid Bus name")
        else {
            panic!("expected runnable CLI");
        };
        assert_eq!(cli.bus_service.as_deref(), Some("comp-test"));
        let ParseOutcome::KmsLive { argv, bus_service } = parse(&[
            "kms-live",
            "--device",
            "/dev/dri/card0",
            "--bus-service",
            "comp-seat-test",
            "--connector",
            "eDP-1",
        ])
        .expect("valid KMS Bus name") else {
            panic!("expected live interlock arguments");
        };
        assert_eq!(bus_service.as_deref(), Some("comp-seat-test"));
        assert!(!argv.iter().any(|arg| arg == "--bus-service"));
        assert!(parse(&["--nested", "--bus-service", "Comp"]).is_err());
        assert!(parse(&["--nested", "--bus-service", "c"]).is_err());
    }

    #[cfg(not(feature = "bus"))]
    #[test]
    fn bus_service_flag_fails_clearly_without_the_feature() {
        assert!(
            parse(&["--nested", "--bus-service", "comp-test"])
                .is_err_and(|error| error.contains("requires the compositor 'bus' feature"))
        );
    }

    #[test]
    fn no_keybindings_disables_interception_at_startup() {
        let ParseOutcome::Run(cli) =
            parse(&["--nested", "--no-keybindings"]).expect("valid arguments")
        else {
            panic!("expected runnable CLI");
        };
        assert!(!cli.keybindings_enabled);
    }

    #[test]
    fn nested_ssd_cli_accepts_default_explicit_on_off_and_chrome() {
        let ParseOutcome::Run(implicit) = parse(&["--nested"]).expect("default SSD") else {
            panic!("expected runnable CLI");
        };
        assert!(implicit.decoration.enabled);

        let ParseOutcome::Run(default_chrome) =
            parse(&["--nested", "--ssd"]).expect("SSD with default chrome")
        else {
            panic!("expected runnable CLI");
        };
        assert!(default_chrome.decoration.enabled);
        assert_eq!(default_chrome.decoration.theme.style, ChromeStyle::Mac);

        for (name, style) in [
            ("mac", ChromeStyle::Mac),
            ("win11", ChromeStyle::Win11),
            ("cosmix", ChromeStyle::Cosmix),
        ] {
            let ParseOutcome::Run(cli) = parse(&["--nested", "--chrome", name])
                .expect("SSD accepts a built-in chrome style")
            else {
                panic!("expected runnable CLI");
            };
            assert!(cli.decoration.enabled);
            assert_eq!(cli.decoration.theme.style, style);
        }

        let ParseOutcome::Run(disabled) =
            parse(&["--nested", "--no-ssd"]).expect("explicit SSD opt-out")
        else {
            panic!("expected runnable CLI");
        };
        assert!(!disabled.decoration.enabled);
    }

    #[test]
    fn nested_ssd_cli_rejects_conflicts_duplicates_and_invalid_chrome() {
        assert!(
            parse(&["--nested", "--ssd", "--chrome", "unknown"])
                .is_err_and(|error| error.contains("unknown chrome style"))
        );
        assert!(
            parse(&["--nested", "--ssd", "--ssd"])
                .is_err_and(|error| error.contains("only be supplied once"))
        );
        assert!(
            parse(&["--nested", "--ssd", "--chrome", "mac", "--chrome", "win11"])
                .is_err_and(|error| error.contains("only be supplied once"))
        );
        assert!(
            parse(&["--nested", "--no-ssd", "--no-ssd"])
                .is_err_and(|error| error == "--no-ssd may only be supplied once")
        );
        for arguments in [
            &["--nested", "--ssd", "--no-ssd"][..],
            &["--nested", "--no-ssd", "--ssd"][..],
        ] {
            assert!(
                parse(arguments)
                    .is_err_and(|error| error == "--ssd and --no-ssd are mutually exclusive")
            );
        }
        for arguments in [
            &["--nested", "--no-ssd", "--chrome", "mac"][..],
            &["--nested", "--chrome", "mac", "--no-ssd"][..],
        ] {
            assert!(
                parse(arguments)
                    .is_err_and(|error| error == "--no-ssd cannot be combined with --chrome")
            );
        }
    }

    #[test]
    fn same_frame_focus_loss_and_gain_still_queue_an_ordered_keyboard_reset() {
        let (mut app, window) = collector_app();
        let key_event = |key_code| {
            WindowEvent::KeyboardInput(bevy::input::keyboard::KeyboardInput {
                key_code,
                logical_key: bevy::input::keyboard::Key::Character("unused".into()),
                state: BevyButtonState::Pressed,
                repeat: false,
                window,
                text: None,
            })
        };

        app.world_mut().write_message(key_event(KeyCode::SuperLeft));
        app.world_mut().write_message(key_event(KeyCode::KeyQ));
        app.world_mut()
            .write_message(WindowEvent::WindowFocused(bevy::window::WindowFocused {
                window,
                focused: false,
            }));
        app.world_mut()
            .write_message(WindowEvent::WindowFocused(bevy::window::WindowFocused {
                window,
                focused: true,
            }));
        app.update();

        // XKB keycodes, not evdev codes: the host reports evdev 125 and 16, and
        // `HostInput::key_from_evdev` is the one place the constant 8 offset is
        // applied. A regression that dropped or doubled it lands here.
        assert!(matches!(
            app.world().resource::<HostInputQueue>().pending.as_slice(),
            [
                HostInput::Key {
                    keycode: super_left,
                    state: HostButtonState::Pressed,
                    ..
                },
                HostInput::Key {
                    keycode: key_q,
                    state: HostButtonState::Pressed,
                    ..
                },
                HostInput::KeyboardFocusLost,
            ] if super_left.raw() == 133 && key_q.raw() == 24
        ));
        assert!(
            app.world().resource::<HostInputQueue>().keyboard_focused,
            "the same-batch focus gain re-arms later real keyboard input"
        );
    }

    #[test]
    fn pump_wayland_turns_the_exit_ecs_action_into_success_in_a_bare_world() {
        let mut world = World::new();
        world.init_resource::<Messages<AppExit>>();
        world.insert_resource(WaylandRuntime::with_test_ecs_action(
            EcsAction::ExitNestedCompositor,
        ));

        pump_wayland(&mut world);

        assert_eq!(
            world
                .resource_mut::<Messages<AppExit>>()
                .drain()
                .collect::<Vec<_>>(),
            [AppExit::Success]
        );
    }

    fn app_with_displayed_dmabuf() -> (App, std::sync::mpsc::Receiver<u64>) {
        use std::{fs::File, sync::mpsc};

        use cosmix_wgpu_dmabuf::{DmabufBufferId, DmabufDescriptor, DmabufPlane, DmabufRelease};
        use smithay::backend::allocator::{Fourcc, Modifier};

        let (protocol_release, protocol_side) = mpsc::channel();
        let mut app = App::new();
        app.init_resource::<Assets<Image>>()
            .init_resource::<ImportedDmabufImages>();
        NestedDmabufTeardownGuard::install(&mut app);
        let importer = app.world().resource::<ImportedDmabufImages>().clone();
        let image = importer
            .import(
                &mut app.world_mut().resource_mut::<Assets<Image>>(),
                DmabufBufferId(73),
                true,
                DmabufDescriptor {
                    width: 8,
                    height: 8,
                    fourcc: Fourcc::Argb8888 as u32,
                    modifier: u64::from(Modifier::Linear),
                    planes: vec![DmabufPlane {
                        fd: File::open("/dev/null")
                            .expect("/dev/null is available")
                            .into(),
                        offset: 0,
                        stride: 32,
                    }],
                },
                DmabufRelease::Explicit(Box::new(move || {
                    protocol_release
                        .send(73_u64)
                        .expect("protocol-side release receiver remains live");
                })),
            )
            .expect("displayed DMA-BUF use is registered");
        app.world_mut().spawn(Sprite::from_image(image));
        drop(importer);
        (app, protocol_side)
    }

    fn assert_no_teardown_release(protocol_side: &std::sync::mpsc::Receiver<u64>) {
        assert!(
            matches!(
                protocol_side.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty
                    | std::sync::mpsc::TryRecvError::Disconnected)
            ),
            "nested App drop must not reach the protocol release seam during teardown"
        );
    }

    #[test]
    fn ecs_action_app_exit_suppresses_release_for_displayed_dmabuf() {
        let (mut app, protocol_side) = app_with_displayed_dmabuf();
        app.insert_resource(WaylandRuntime::with_test_ecs_action(
            EcsAction::ExitNestedCompositor,
        ));

        pump_wayland(app.world_mut());

        assert_eq!(
            app.world_mut()
                .resource_mut::<Messages<AppExit>>()
                .drain()
                .collect::<Vec<_>>(),
            [AppExit::Success]
        );
        drop(app);
        assert_no_teardown_release(&protocol_side);
    }

    #[test]
    fn window_originated_app_exit_suppresses_release_for_displayed_dmabuf() {
        use bevy::window::{PrimaryWindow, WindowCloseRequested};

        let (mut app, protocol_side) = app_with_displayed_dmabuf();
        app.add_plugins(WindowPlugin {
            primary_window: None,
            ..default()
        });
        let window = app
            .world_mut()
            .spawn((Window::default(), PrimaryWindow))
            .id();
        app.world_mut()
            .write_message(WindowCloseRequested { window });

        // WindowPlugin marks the close on one Last pass, despawns on the next,
        // then its default OnPrimaryClosed exit system observes the absence.
        app.update();
        app.update();
        app.update();

        assert_eq!(app.should_exit(), Some(AppExit::Success));
        drop(app);
        assert_no_teardown_release(&protocol_side);
    }

    #[test]
    fn panicking_nested_app_suppresses_release_for_displayed_dmabuf() {
        let (app, protocol_side) = app_with_displayed_dmabuf();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _app = app;
            panic!("injected nested runner failure");
        }));

        assert!(panic.is_err(), "the injected panic must unwind the App");
        assert_no_teardown_release(&protocol_side);
    }

    #[test]
    fn pump_wayland_reports_queued_runtime_failure_before_kms_disconnect() {
        let mut runtime = WaylandRuntime::with_test_runtime_failure_and_disconnected_kms(
            "injected protocol failure",
        );
        let scene_feed = runtime
            .take_client_scene_feed()
            .expect("test runtime owns its scene feed");
        let mut app = App::new();
        app.init_resource::<Assets<Image>>()
            .insert_resource(runtime)
            .insert_resource(scene_feed)
            .add_plugins(CompositorScenePlugin::new(
                INITIAL_WIDTH,
                INITIAL_HEIGHT,
                SceneCursorMode::HostCursor,
            ))
            .add_systems(First, pump_wayland.after(CompositorSceneSet));

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            app.update();
        }))
        .expect_err("queued runtime failure must panic");
        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .expect("panic carries a string message");

        assert_eq!(
            message,
            "Wayland protocol thread failed: injected protocol failure"
        );
    }

    #[test]
    fn pump_wayland_cannot_replace_a_scene_feed_disconnect_failure() {
        let runtime =
            WaylandRuntime::with_test_runtime_failure_and_disconnected_kms("unused queued failure");
        let (scene_sender, scene_feed) = protocol::ClientSceneFeed::test_channel();
        drop(scene_sender);
        let mut app = App::new();
        app.init_resource::<Assets<Image>>()
            .insert_resource(runtime)
            .insert_resource(scene_feed)
            .add_plugins(CompositorScenePlugin::new(
                INITIAL_WIDTH,
                INITIAL_HEIGHT,
                SceneCursorMode::HostCursor,
            ))
            .add_systems(First, pump_wayland.after(CompositorSceneSet));

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            app.update();
        }))
        .expect_err("a dropped protocol event sender must panic");
        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .expect("panic carries a string message");

        assert!(
            app.world().contains_resource::<CompositorSceneFailed>(),
            "the scene failure marker remains set after unwinding"
        );
        assert_eq!(message, "Wayland protocol thread disconnected");
    }

    #[test]
    fn list_bindings_does_not_require_nested_mode_and_is_strict_data() {
        let ParseOutcome::ListBindings {
            keybindings_enabled,
            profile,
        } = parse(&["--list-bindings"]).expect("valid arguments")
        else {
            panic!("expected binding-list CLI");
        };
        assert_eq!(profile, bindings::BindingProfile::Nested);
        let listing =
            bindings::BindingState::for_profile(profile, keybindings_enabled).to_strict_data();
        let status = std::process::Command::new("/opt/cosmix/bin/mix")
            .args([
                "-c",
                "$data = data_parse(env(\"COSMIX_BINDING_TEST_DATA\")); \
                 if type($data) != \"map\" then die(\"binding listing is not a map\") end",
            ])
            .env("COSMIX_BINDING_TEST_DATA", listing)
            .status()
            .expect("run the authoritative Mix strict-data parser");
        assert!(status.success());
    }

    #[test]
    fn list_bindings_can_select_the_kms_live_profile() {
        let ParseOutcome::ListBindings {
            keybindings_enabled,
            profile,
        } = parse(&["--list-bindings", "--binding-profile", "kms-live"])
            .expect("valid KMS-live binding listing")
        else {
            panic!("expected binding-list CLI");
        };
        assert_eq!(profile, bindings::BindingProfile::KmsLive);
        let listing =
            bindings::BindingState::for_profile(profile, keybindings_enabled).to_strict_data();
        for vt in 1..=12 {
            assert!(listing.contains(&format!("\"id\": \"switch-vt-{vt}\"")));
        }
        assert!(!listing.contains("exit-nested-compositor"));
    }

    #[test]
    fn backend_mode_is_required() {
        assert!(parse(&[]).is_err_and(|error| error.contains("--nested or --kms")));
    }

    #[test]
    fn kms_selects_the_kms_backend() {
        let ParseOutcome::Run(cli) = parse(&["--kms"]).expect("valid KMS selection") else {
            panic!("expected runnable CLI");
        };
        assert_eq!(cli.backend, BackendKind::Kms);
    }

    #[test]
    fn nested_and_kms_cannot_be_selected_together() {
        assert!(
            parse(&["--nested", "--kms"]).is_err_and(|error| error.contains("mutually exclusive"))
        );
    }

    #[test]
    fn repeated_nested_selection_remains_idempotent() {
        let ParseOutcome::Run(cli) =
            parse(&["--nested", "--nested"]).expect("nested selection remains idempotent")
        else {
            panic!("expected runnable CLI");
        };
        assert_eq!(cli.backend, BackendKind::Winit);
    }

    #[test]
    fn kms_probe_is_a_standalone_read_only_mode() {
        assert!(matches!(
            parse(&["--kms-probe"]).expect("valid KMS probe selection"),
            ParseOutcome::KmsProbe
        ));
        assert!(
            parse(&["--kms-probe", "--nested"])
                .is_err_and(|error| error.contains("must be supplied by itself"))
        );
        assert!(
            parse(&["--kms-probe", "--socket", "probe"])
                .is_err_and(|error| error.contains("must be supplied by itself"))
        );
    }

    #[test]
    fn kms_watch_is_bounded_and_standalone() {
        assert!(matches!(
            parse(&["--kms-watch", "7"]).expect("valid KMS watch"),
            ParseOutcome::KmsWatch { seconds: 7 }
        ));
        assert!(
            parse(&["--kms-watch", "0"]).is_err_and(|error| error.contains("between 1 and 3600"))
        );
        assert!(
            parse(&["--kms-watch", "3601"])
                .is_err_and(|error| error.contains("between 1 and 3600"))
        );
        assert!(
            parse(&["--kms-watch", "7", "--nested"])
                .is_err_and(|error| error.contains("must be supplied by itself"))
        );
        assert!(
            parse(&["--kms-watch", "7", "--kms-probe"])
                .is_err_and(|error| error.contains("must be supplied by itself"))
        );
    }

    #[test]
    fn kms_live_arguments_are_delegated_to_the_pure_interlock() {
        let ParseOutcome::KmsLive { argv, .. } = parse(&[
            "kms-live",
            "--device",
            "/dev/dri/card0",
            "--connector",
            "eDP-1",
        ])
        .expect("live intent is parsed by its fail-closed interlock") else {
            panic!("expected live interlock arguments");
        };

        assert_eq!(argv.first().and_then(|arg| arg.to_str()), Some("kms-live"));
    }

    #[test]
    fn kms_live_help_uses_the_shared_usage_text() {
        assert!(matches!(
            parse(&["kms-live", "--help"]).expect("kms-live help"),
            ParseOutcome::Help
        ));
        assert!(matches!(
            parse(&["kms-live", "-h"]).expect("kms-live short help"),
            ParseOutcome::Help
        ));
    }

    #[test]
    fn misplaced_kms_live_is_still_delegated_for_a_stable_refusal() {
        let ParseOutcome::KmsLive { argv, .. } =
            parse(&["--nested", "kms-live"]).expect("live intent must fail in the interlock")
        else {
            panic!("expected live interlock arguments");
        };

        assert_eq!(
            backend::kms_live::refusal_reason_for_test(&argv),
            "kms-live-subcommand-not-first"
        );
    }

    #[test]
    fn kms_live_remains_a_valid_value_for_an_existing_option() {
        let ParseOutcome::Run(cli) =
            parse(&["--nested", "--socket", "kms-live"]).expect("valid socket value")
        else {
            panic!("expected runnable CLI");
        };

        assert_eq!(cli.socket, "kms-live");
    }

    #[test]
    fn common_keyboard_codes_match_linux_evdev() {
        assert_eq!(evdev_keycode(KeyCode::KeyA), Some(30));
        assert_eq!(evdev_keycode(KeyCode::Enter), Some(28));
        assert_eq!(evdev_keycode(KeyCode::ArrowLeft), Some(105));
        assert_eq!(evdev_keycode(KeyCode::ControlRight), Some(97));
    }

    fn wheel(unit: MouseScrollUnit, x: f32, y: f32, phase: TouchPhase) -> MouseWheel {
        MouseWheel {
            unit,
            x,
            y,
            window: Entity::PLACEHOLDER,
            phase,
        }
    }

    fn axes(input: Option<HostInput>) -> (Option<HostAxis>, Option<HostAxis>) {
        match input {
            Some(HostInput::PointerAxis {
                horizontal,
                vertical,
                ..
            }) => (horizontal, vertical),
            other => panic!("expected a pointer axis event, got {other:?}"),
        }
    }

    #[test]
    fn nested_vertical_scroll_leaves_the_untouched_axis_absent() {
        let mut scrolling = ScrollingAxes::default();
        let (horizontal, vertical) = axes(host_axis_from_wheel(
            &wheel(MouseScrollUnit::Pixel, 0.0, -3.0, TouchPhase::Moved),
            &mut scrolling,
            7,
        ));
        // Bevy reports both components on every message. A zero component is an
        // axis that did not move; reporting it as a zero would reach the client
        // as a `wl_pointer.axis_stop` for a sequence it was never told started.
        assert_eq!(
            horizontal, None,
            "an axis that did not move is absent, not a reported zero"
        );
        assert_eq!(
            vertical,
            Some(HostAxis {
                amount: 3.0,
                v120: None
            }),
            "Bevy reports positive for up, Wayland positive for down"
        );
    }

    #[test]
    fn nested_finger_lift_stops_only_the_axis_that_was_scrolling() {
        let mut scrolling = ScrollingAxes::default();
        host_axis_from_wheel(
            &wheel(MouseScrollUnit::Pixel, 0.0, -3.0, TouchPhase::Moved),
            &mut scrolling,
            7,
        )
        .expect("a moving axis produces a frame");

        let (horizontal, vertical) = axes(host_axis_from_wheel(
            &wheel(MouseScrollUnit::Pixel, 0.0, 0.0, TouchPhase::Ended),
            &mut scrolling,
            8,
        ));
        assert_eq!(
            horizontal, None,
            "the horizontal axis never scrolled, so it has no sequence to end"
        );
        assert_eq!(
            vertical,
            Some(HostAxis {
                amount: 0.0,
                v120: None
            }),
            "the lift is the only thing that reports a zero, and it ends the axis that scrolled"
        );
    }

    #[test]
    fn nested_lift_with_nothing_in_flight_emits_nothing() {
        let mut scrolling = ScrollingAxes::default();
        assert!(
            host_axis_from_wheel(
                &wheel(MouseScrollUnit::Pixel, 0.0, 0.0, TouchPhase::Ended),
                &mut scrolling,
                7,
            )
            .is_none(),
            "a lift that ends no sequence is not a scroll event"
        );
    }

    #[test]
    fn nested_second_lift_does_not_stop_the_axis_twice() {
        let mut scrolling = ScrollingAxes::default();
        host_axis_from_wheel(
            &wheel(MouseScrollUnit::Pixel, 0.0, -3.0, TouchPhase::Moved),
            &mut scrolling,
            7,
        )
        .expect("a moving axis produces a frame");
        host_axis_from_wheel(
            &wheel(MouseScrollUnit::Pixel, 0.0, 0.0, TouchPhase::Ended),
            &mut scrolling,
            8,
        )
        .expect("the lift ends the vertical axis");
        assert!(
            host_axis_from_wheel(
                &wheel(MouseScrollUnit::Pixel, 0.0, 0.0, TouchPhase::Ended),
                &mut scrolling,
                9,
            )
            .is_none(),
            "the sequence is already closed, so a repeated lift carries nothing"
        );
    }

    #[test]
    fn nested_idle_scroll_message_emits_nothing() {
        let mut scrolling = ScrollingAxes::default();
        assert!(
            host_axis_from_wheel(
                &wheel(MouseScrollUnit::Pixel, 0.0, 0.0, TouchPhase::Moved),
                &mut scrolling,
                7,
            )
            .is_none(),
            "a message describing no movement and no lift needs no frame"
        );
    }

    #[test]
    fn nested_wheel_line_carries_detents_on_the_axis_that_moved() {
        let mut scrolling = ScrollingAxes::default();
        let (horizontal, vertical) = axes(host_axis_from_wheel(
            &wheel(MouseScrollUnit::Line, 0.0, -1.0, TouchPhase::Moved),
            &mut scrolling,
            7,
        ));
        assert_eq!(horizontal, None, "one wheel click turns one axis");
        assert_eq!(
            vertical,
            Some(HostAxis {
                amount: 15.0,
                v120: Some(120)
            }),
            "one detent is 120 v120 units and 15 axis units, the scale libinput defines"
        );
    }

    #[test]
    fn nested_frame_carries_the_source_timestamp_and_direction_the_unit_implies() {
        // The other tests read the axes through a helper that discards the rest
        // of the envelope. Source in particular is not cosmetic: it decides
        // whether the compositor is allowed to send `axis_stop` at all.
        let mut scrolling = ScrollingAxes::default();

        let Some(HostInput::PointerAxis {
            source,
            relative_direction,
            time,
            ..
        }) = host_axis_from_wheel(
            &wheel(MouseScrollUnit::Line, 0.0, -1.0, TouchPhase::Moved),
            &mut scrolling,
            7,
        )
        else {
            panic!("a wheel click produces a pointer axis frame");
        };
        assert_eq!(source, AxisSource::Wheel, "a line unit is a detented wheel");
        assert_eq!(time, 7, "the frame carries the timestamp it was given");
        assert_eq!(
            relative_direction,
            (
                AxisRelativeDirection::Identical,
                AxisRelativeDirection::Identical
            ),
            "Bevy exposes no natural-scrolling state, so the direction is reported as physical"
        );

        let Some(HostInput::PointerAxis { source, time, .. }) = host_axis_from_wheel(
            &wheel(MouseScrollUnit::Pixel, 0.0, -3.0, TouchPhase::Moved),
            &mut scrolling,
            9,
        ) else {
            panic!("a drag produces a pointer axis frame");
        };
        assert_eq!(
            source,
            AxisSource::Continuous,
            "a pixel unit may be a trackpad or a high-resolution wheel, so it claims neither"
        );
        assert_eq!(time, 9);
    }

    #[test]
    fn nested_lift_keeps_the_axis_that_is_still_moving() {
        let mut scrolling = ScrollingAxes::default();
        host_axis_from_wheel(
            &wheel(MouseScrollUnit::Pixel, -6.0, -3.0, TouchPhase::Moved),
            &mut scrolling,
            7,
        )
        .expect("a diagonal drag produces a frame");

        // winit raises `Ended` when *either* axis stops, and still carries both
        // deltas. Reading the phase as "the gesture ended" rather than asking
        // each axis its own value dropped the surviving axis's motion and
        // fabricated a stop for it in the same frame.
        //
        // This is the shape `wl_pointer.frame` names: an axis and an axis_stop
        // in one frame means one axis stopped while the other continues. So the
        // zero component is the stop and the non-zero one is not — that is the
        // protocol's own rule, not an inference from this transport. Stopping
        // both here is the alternative that reintroduces the defect.
        let (horizontal, vertical) = axes(host_axis_from_wheel(
            &wheel(MouseScrollUnit::Pixel, -6.0, 0.0, TouchPhase::Ended),
            &mut scrolling,
            8,
        ));
        assert_eq!(
            horizontal,
            Some(HostAxis {
                amount: 6.0,
                v120: None
            }),
            "an axis with a value is still moving, whatever the phase says about the other"
        );
        assert_eq!(
            vertical,
            Some(HostAxis {
                amount: 0.0,
                v120: None
            }),
            "only the axis that reported zero is the one that stopped"
        );

        // The surviving axis is still in flight, so its own stop is still owed
        // and still arrives.
        let (horizontal, vertical) = axes(host_axis_from_wheel(
            &wheel(MouseScrollUnit::Pixel, 0.0, 0.0, TouchPhase::Ended),
            &mut scrolling,
            9,
        ));
        assert_eq!(
            horizontal,
            Some(HostAxis {
                amount: 0.0,
                v120: None
            }),
            "the axis that kept moving stops when it finally reports zero"
        );
        assert_eq!(vertical, None, "the vertical axis already stopped");
    }

    #[test]
    fn nested_wheel_does_not_arm_a_stop_for_a_later_gesture() {
        let mut scrolling = ScrollingAxes::default();
        host_axis_from_wheel(
            &wheel(MouseScrollUnit::Line, 0.0, -1.0, TouchPhase::Moved),
            &mut scrolling,
            7,
        )
        .expect("a wheel click produces a frame");

        // A wheel has no gesture and never reaches an ending phase, so it must
        // not mark an axis in flight. If it did, the mark would never clear and
        // the next trackpad lift would stop an axis whose motion came from the
        // wheel — reported as `Continuous`, which the wheel never was.
        assert!(
            host_axis_from_wheel(
                &wheel(MouseScrollUnit::Pixel, 0.0, 0.0, TouchPhase::Ended),
                &mut scrolling,
                8,
            )
            .is_none(),
            "a wheel click leaves no continuous sequence for a lift to end"
        );
    }

    #[test]
    fn nested_started_phase_discards_the_previous_gesture() {
        let mut scrolling = ScrollingAxes::default();
        host_axis_from_wheel(
            &wheel(MouseScrollUnit::Pixel, 0.0, -3.0, TouchPhase::Moved),
            &mut scrolling,
            7,
        )
        .expect("a vertical drag produces a frame");

        // A gesture that never got its lift must not leave its axes armed for
        // the next one. This is the backstop; in practice winit's Wayland
        // pointer holds the phase at `Moved`, so `end_scrolling_gesture` on
        // pointer leave is what really clears it. Not on focus loss — the
        // pointer can stay and keep scrolling while the keyboard leaves, which
        // `nested_keyboard_focus_loss_leaves_a_live_gesture_alone` pins.
        host_axis_from_wheel(
            &wheel(MouseScrollUnit::Pixel, -2.0, 0.0, TouchPhase::Started),
            &mut scrolling,
            8,
        )
        .expect("a new horizontal gesture produces a frame");

        let (horizontal, vertical) = axes(host_axis_from_wheel(
            &wheel(MouseScrollUnit::Pixel, 0.0, 0.0, TouchPhase::Ended),
            &mut scrolling,
            9,
        ));
        assert_eq!(
            horizontal,
            Some(HostAxis {
                amount: 0.0,
                v120: None
            }),
            "the new gesture's axis stops"
        );
        assert_eq!(
            vertical, None,
            "the abandoned gesture's axis belongs to a sequence this lift did not end"
        );
    }

    #[test]
    fn nested_cancelled_gesture_stops_without_treating_its_deltas_as_motion() {
        let mut scrolling = ScrollingAxes::default();
        host_axis_from_wheel(
            &wheel(MouseScrollUnit::Pixel, 0.0, -3.0, TouchPhase::Moved),
            &mut scrolling,
            7,
        )
        .expect("a vertical drag produces a frame");

        // A cancellation is terminal. Reading its components as motion would
        // re-arm the axis and owe a stop that nothing is left to send — the
        // same stranded-mark defect the wheel path was fixed for.
        let (horizontal, vertical) = axes(host_axis_from_wheel(
            &wheel(MouseScrollUnit::Pixel, -4.0, -4.0, TouchPhase::Canceled),
            &mut scrolling,
            8,
        ));
        assert_eq!(
            horizontal, None,
            "an axis that never scrolled has nothing to cancel"
        );
        assert_eq!(
            vertical,
            Some(HostAxis {
                amount: 0.0,
                v120: None
            }),
            "the axis that was scrolling stops, and its delta is not motion"
        );
        assert!(
            end_scrolling_gesture(&mut scrolling, 9).is_none(),
            "nothing is left in flight after a cancellation"
        );
    }

    /// A Bevy app wired to nothing but `collect_host_input`, so a written
    /// `WindowEvent` reaches the translation the same way winit's forwarded
    /// batch does.
    fn collector_app() -> (App, Entity) {
        let mut app = App::new();
        app.init_resource::<HostInputQueue>()
            .add_message::<CursorMoved>()
            .add_message::<MouseButtonInput>()
            .add_message::<WindowEvent>()
            .add_message::<WindowResized>()
            .add_message::<WindowBackendScaleFactorChanged>()
            .add_systems(Update, collect_host_input);
        let window = app.world_mut().spawn_empty().id();
        (app, window)
    }

    fn wheel_event(window: Entity, x: f32, y: f32, phase: TouchPhase) -> WindowEvent {
        WindowEvent::MouseWheel(MouseWheel {
            unit: MouseScrollUnit::Pixel,
            x,
            y,
            window,
            phase,
        })
    }

    #[test]
    fn nested_pointer_leave_ends_the_gesture_that_will_never_report_its_lift() {
        let (mut app, window) = collector_app();

        app.world_mut()
            .write_message(wheel_event(window, 0.0, -3.0, TouchPhase::Moved));
        app.update();
        assert!(
            app.world()
                .resource::<HostInputQueue>()
                .scrolling_axes
                .vertical,
            "the drag is in flight"
        );

        app.world_mut()
            .resource_mut::<HostInputQueue>()
            .pending
            .clear();
        app.world_mut()
            .write_message(WindowEvent::CursorLeft(bevy::window::CursorLeft { window }));
        app.update();

        // winit does not reset its scroll phase when the pointer leaves, so the
        // next gesture arrives as `Moved` and never trips the `Started` reset.
        // Without this the mark survives the leave and the next lift stops an
        // axis that was not scrolling.
        let queue = app.world().resource::<HostInputQueue>();
        assert!(
            matches!(
                queue.pending.as_slice(),
                [HostInput::PointerAxis {
                    horizontal: None,
                    vertical: Some(HostAxis {
                        amount: 0.0,
                        v120: None
                    }),
                    source: AxisSource::Continuous,
                    ..
                }]
            ),
            "the leave stops the axis that was scrolling, got {:?}",
            queue.pending
        );
        assert!(
            !queue.scrolling_axes.vertical,
            "and nothing is left armed for the next gesture"
        );
    }

    #[test]
    fn nested_keyboard_focus_loss_leaves_a_live_gesture_alone() {
        let (mut app, window) = collector_app();

        app.world_mut()
            .write_message(wheel_event(window, 0.0, -3.0, TouchPhase::Moved));
        app.update();
        app.world_mut()
            .resource_mut::<HostInputQueue>()
            .pending
            .clear();

        // Keyboard focus and pointer focus are independent under Wayland: the
        // pointer can keep scrolling in our surface while the keyboard moves to
        // another window. Stopping the gesture here would cut a live scroll and
        // put an unearned `axis_stop` on the wire.
        app.world_mut()
            .write_message(WindowEvent::WindowFocused(bevy::window::WindowFocused {
                window,
                focused: false,
            }));
        app.update();

        let queue = app.world().resource::<HostInputQueue>();
        assert!(
            matches!(queue.pending.as_slice(), [HostInput::KeyboardFocusLost]),
            "focus loss resets the keyboard and touches nothing else, got {:?}",
            queue.pending
        );
        assert!(
            queue.scrolling_axes.vertical,
            "and the gesture is still in flight"
        );
    }

    #[test]
    fn nested_pointer_leave_does_not_stop_a_scroll_that_arrived_after_it() {
        let (mut app, window) = collector_app();

        // One slow nested frame carrying a whole leave-and-return: the pointer
        // leaves mid-gesture and a fresh gesture starts after it comes back.
        // Bevy writes wheel messages to both `Messages<MouseWheel>` and the
        // unified `WindowEvent` stream, but only the unified one keeps them in
        // OS order against the leave. Draining wheels first would apply the
        // later scroll before the earlier leave, stop the axis it just armed,
        // and hand the client the two frames backwards.
        let world = app.world_mut();
        world.write_message(wheel_event(window, 0.0, -3.0, TouchPhase::Moved));
        world.write_message(WindowEvent::CursorLeft(bevy::window::CursorLeft { window }));
        world.write_message(WindowEvent::CursorEntered(bevy::window::CursorEntered {
            window,
        }));
        world.write_message(wheel_event(window, 0.0, -4.0, TouchPhase::Moved));
        app.update();

        let queue = app.world().resource::<HostInputQueue>();
        let amounts: Vec<_> = queue
            .pending
            .iter()
            .map(|input| match input {
                HostInput::PointerAxis { vertical, .. } => vertical.map(|axis| axis.amount),
                other => panic!("expected only pointer axes, got {other:?}"),
            })
            .collect();
        assert_eq!(
            amounts,
            vec![Some(3.0), Some(0.0), Some(4.0)],
            "drag, then the leave's stop, then the new drag — in that order"
        );
        assert!(
            queue.scrolling_axes.vertical,
            "the gesture that arrived last is the one left in flight"
        );
    }

    #[test]
    fn nested_pointer_leave_with_no_gesture_in_flight_emits_no_axis_frame() {
        let mut scrolling = ScrollingAxes::default();

        // The pointer leaves constantly and gestures are rare. Synthesising an
        // empty axis frame on every leave would put a stop on the wire for an
        // axis that never moved.
        assert!(end_scrolling_gesture(&mut scrolling, 7).is_none());
    }
}
