//! The Bus citizen daemon (`cosmix-musicd serve`).
//!
//! Verbs (all under the `musicd.` prefix): `render` (MIDI file → WAV, headless),
//! `play` / `stop` (to the speakers, device-gated), `status`, `soundfonts`,
//! `load_soundfont`, plus the SPEC-07 `props.*` surface. Wiring mirrors
//! cosmix-indexd: supervised reconnect loop, `handle_bus_commands` dispatch,
//! and a 1 Hz `world.musicd` publisher (see `world.rs`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use rustysynth::SoundFont;
use serde::Deserialize;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use cosmix_config::MusicdSettings;

use crate::play::{self, PlaybackEngine};
use crate::props::{self, MusicdPropsSnapshot};
use crate::render::{self, RenderOptions};
use crate::{fetch, synth, world};

/// System-mode config dir; the systemd unit's `config.conf.mix` lives here.
const SYSTEM_CFG_DIR: &str = "/etc/cosmix/musicd";

/// All daemon state, behind a tokio `Mutex`. The non-`Send` cpal stream is NOT
/// here — it lives on the playback engine's dedicated thread (see `play.rs`);
/// this holds only the engine handle and lock-free status is read through it.
pub struct AppState {
    pub settings: MusicdSettings,
    pub soundfont_path: PathBuf,
    pub soundfont: Option<Arc<SoundFont>>,
    pub soundfont_error: String,
    pub started: Instant,
    pub started_at_iso: String,
    pub renders_total: u64,
    pub plays_total: u64,
    pub device_available: bool,
    pub current_midi: String,
    pub engine: Arc<PlaybackEngine>,
}

/// Either a cached SoundFont handle or a path to load off the async runtime.
enum SfSource {
    Cached(Arc<SoundFont>),
    Load(PathBuf),
}

// ---------------------------------------------------------------------------
// Bus request surface
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Request {
    Render(RenderReq),
    Play(PlayReq),
    Stop,
    Status,
    Soundfonts,
    LoadSoundfont(LoadSfReq),
}

#[derive(Deserialize)]
struct RenderReq {
    midi_path: String,
    #[serde(default)]
    out_path: Option<String>,
    #[serde(default)]
    sample_rate: Option<u32>,
    #[serde(default)]
    gain: Option<f32>,
    #[serde(default)]
    soundfont: Option<String>,
    /// "stereo" (default) | "mono" | "left" | "right" — same as the CLI.
    #[serde(default)]
    channels: Option<String>,
    /// "wav16" (default) | "wav24" | "wavf32" | "flac24" — same as the CLI.
    #[serde(default)]
    format: Option<String>,
}

/// Parse the Bus `channels` string with the same vocabulary as the CLI.
fn parse_channels(s: &str) -> Result<render::Channels, String> {
    match s {
        "stereo" => Ok(render::Channels::Stereo),
        "mono" => Ok(render::Channels::Mono),
        "left" => Ok(render::Channels::Left),
        "right" => Ok(render::Channels::Right),
        other => Err(format!(
            "invalid channels '{other}' (stereo|mono|left|right)"
        )),
    }
}

/// Parse the Bus `format` string with the same vocabulary as the CLI.
fn parse_format(s: &str) -> Result<render::RenderFormat, String> {
    match s {
        "wav16" => Ok(render::RenderFormat::Wav16),
        "wav24" => Ok(render::RenderFormat::Wav24),
        "wavf32" | "wav-f32" => Ok(render::RenderFormat::WavF32),
        "flac24" => Ok(render::RenderFormat::Flac24),
        other => Err(format!(
            "invalid format '{other}' (wav16|wav24|wavf32|flac24)"
        )),
    }
}

#[derive(Deserialize)]
struct PlayReq {
    midi_path: String,
    #[serde(default)]
    gain: Option<f32>,
    #[serde(default)]
    soundfont: Option<String>,
}

#[derive(Deserialize)]
struct LoadSfReq {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    path: Option<String>,
}

fn json_error(msg: &str) -> String {
    serde_json::json!({ "error": msg }).to_string()
}

/// Expand a leading `~/` against `$HOME` (Bus callers pass shell-style paths).
fn expand_path(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{home}/{rest}");
    }
    p.to_string()
}

/// Join `name` under `base`, rejecting anything that escapes `base` (absolute
/// paths, `..`, root/prefix components). Bus callers sit in the WG trust domain
/// but are NOT trusted to make the daemon read/write arbitrary files — a custom
/// SoundFont or a named render goes *in* the state dir, referenced by a relative
/// name. Subdirectories are allowed; traversal is not.
fn confined_path(base: &Path, name: &str) -> Result<PathBuf, String> {
    use std::path::Component;
    let candidate = Path::new(name);
    if candidate.is_absolute() {
        return Err(format!(
            "path must be relative to {}: {name}",
            base.display()
        ));
    }
    for comp in candidate.components() {
        if !matches!(comp, Component::Normal(_)) {
            return Err(format!(
                "path may not contain '..' or absolute segments: {name}"
            ));
        }
    }
    if candidate.as_os_str().is_empty() {
        return Err("path is empty".to_string());
    }
    Ok(base.join(candidate))
}

/// Pick the SoundFont source: a named override confined to the state dir (loaded
/// off-lock) or the cached default. An override that names the already-loaded
/// default reuses the cached handle (no 150 MB reparse). Errors as a String.
fn pick_soundfont(g: &AppState, override_name: Option<&str>) -> Result<SfSource, String> {
    match override_name {
        Some(name) => {
            // Common case: override names the loaded default → reuse the cache.
            if name == g.settings.soundfont_name
                && let Some(a) = g.soundfont.clone()
            {
                return Ok(SfSource::Cached(a));
            }
            let path = confined_path(Path::new(&g.settings.state_dir), name)?;
            Ok(SfSource::Load(path))
        }
        None => g.soundfont.clone().map(SfSource::Cached).ok_or_else(|| {
            let why = if g.soundfont_error.is_empty() {
                "not yet loaded".to_string()
            } else {
                g.soundfont_error.clone()
            };
            format!("soundfont unavailable: {why}")
        }),
    }
}

fn materialize_sf(src: SfSource) -> Result<Arc<SoundFont>> {
    match src {
        SfSource::Cached(a) => Ok(a),
        SfSource::Load(p) => synth::load_soundfont(&p),
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn handle_render(req: RenderReq, state: &Arc<Mutex<AppState>>) -> String {
    let midi_path = expand_path(&req.midi_path);
    if !Path::new(&midi_path).is_file() {
        return json_error(&format!("midi file not found: {midi_path}"));
    }

    let channels = match req.channels.as_deref().map(parse_channels).transpose() {
        Ok(c) => c.unwrap_or(render::Channels::Stereo),
        Err(e) => return json_error(&e),
    };
    let format = match req.format.as_deref().map(parse_format).transpose() {
        Ok(f) => f.unwrap_or(render::RenderFormat::Wav16),
        Err(e) => return json_error(&e),
    };

    let (sf_src, opts, out_path) = {
        let g = state.lock().await;
        let sample_rate = req.sample_rate.unwrap_or(g.settings.sample_rate);
        let gain = req.gain.unwrap_or(g.settings.gain);
        let opts = RenderOptions {
            sample_rate: sample_rate as i32,
            gain,
            max_polyphony: g.settings.max_polyphony as usize,
            reverb_chorus: true,
            channels,
            format,
        };
        // Renders always land under state_dir/renders — the daemon is sandboxed
        // out of $HOME and the wider FS, and a caller-supplied `out_path` is
        // confined to a relative name within that dir (no `..`/absolute escape).
        let renders = Path::new(&g.settings.state_dir).join("renders");
        let out_path = match req.out_path.as_deref() {
            Some(p) => match confined_path(&renders, p) {
                Ok(pp) => pp,
                Err(e) => return json_error(&e),
            },
            None => {
                let stem = Path::new(&midi_path)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("render");
                let ext = if format == render::RenderFormat::Flac24 {
                    "flac"
                } else {
                    "wav"
                };
                renders.join(format!("{stem}.{ext}"))
            }
        };
        match pick_soundfont(&g, req.soundfont.as_deref()) {
            Ok(src) => (src, opts, out_path),
            Err(e) => return json_error(&e),
        }
    };

    let midi_p = midi_path.clone();
    let out_p = out_path.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<render::RenderReport> {
        if let Some(parent) = out_p.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let sf = materialize_sf(sf_src)?;
        let midi = synth::load_midi(&midi_p)?;
        let (l, r, report) = render::render_to_buffers(&sf, &midi, &opts)?;
        render::write_render(&out_p, &l, &r, opts.sample_rate, opts.channels, opts.format)?;
        Ok(report)
    })
    .await;

    match result {
        Ok(Ok(report)) => {
            state.lock().await.renders_total += 1;
            serde_json::json!({
                "rendered": true,
                "out_path": out_path.display().to_string(),
                "duration_s": report.duration_s,
                "frames": report.frames,
                "sample_rate": report.sample_rate,
                "peak": report.peak,
                "clipped": report.clipped,
            })
            .to_string()
        }
        Ok(Err(e)) => json_error(&format!("render failed: {e}")),
        Err(e) => json_error(&format!("render task panicked: {e}")),
    }
}

async fn handle_play(req: PlayReq, state: &Arc<Mutex<AppState>>) -> String {
    let midi_path = expand_path(&req.midi_path);
    if !Path::new(&midi_path).is_file() {
        return json_error(&format!("midi file not found: {midi_path}"));
    }

    let (engine, sf_src, gain) = {
        let g = state.lock().await;
        if !g.device_available {
            return json_error("no audio output device (headless); use render");
        }
        let gain = req.gain.unwrap_or(g.settings.gain);
        match pick_soundfont(&g, req.soundfont.as_deref()) {
            Ok(src) => (g.engine.clone(), src, gain),
            Err(e) => return json_error(&e),
        }
    };

    let midi_p = midi_path.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<play::PlayInfo> {
        let sf = materialize_sf(sf_src)?;
        let midi = synth::load_midi(&midi_p)?;
        engine.play(sf, midi, gain)
    })
    .await;

    match result {
        Ok(Ok(info)) => {
            let mut g = state.lock().await;
            g.plays_total += 1;
            g.current_midi = midi_path.clone();
            serde_json::json!({
                "playing": true,
                "duration_s": info.duration_s,
                "sample_rate": info.sample_rate,
                "midi": midi_path,
            })
            .to_string()
        }
        Ok(Err(e)) => json_error(&format!("play failed: {e}")),
        Err(e) => json_error(&format!("play task panicked: {e}")),
    }
}

async fn handle_stop(state: &Arc<Mutex<AppState>>) -> String {
    let engine = { state.lock().await.engine.clone() };
    let was = tokio::task::spawn_blocking(move || engine.stop())
        .await
        .unwrap_or(false);
    serde_json::json!({ "stopped": true, "was_playing": was }).to_string()
}

async fn handle_status(state: &Arc<Mutex<AppState>>) -> String {
    let s = collect_props(state).await;
    serde_json::json!({
        "soundfont_loaded": s.soundfont_loaded,
        "soundfont": s.soundfont_name,
        "soundfont_error": s.soundfont_error,
        "device_available": s.device_available,
        "playing": s.playing,
        "current_midi": s.current_midi,
        "position_s": s.position_s,
        "duration_s": s.duration_s,
        "renders_total": s.renders_total,
        "plays_total": s.plays_total,
        "uptime_s": s.uptime_s,
        "sample_rate": s.sample_rate,
    })
    .to_string()
}

async fn handle_soundfonts(state: &Arc<Mutex<AppState>>) -> String {
    let g = state.lock().await;
    let dir = PathBuf::from(&g.settings.state_dir);
    let mut available = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let p = e.path();
            let is_sf2 = p
                .extension()
                .and_then(|x| x.to_str())
                .map(|x| x.eq_ignore_ascii_case("sf2"))
                .unwrap_or(false);
            if is_sf2 {
                let bytes = e.metadata().map(|m| m.len()).unwrap_or(0);
                available.push(serde_json::json!({
                    "name": p.file_name().and_then(|n| n.to_str()).unwrap_or(""),
                    "path": p.display().to_string(),
                    "bytes": bytes,
                }));
            }
        }
    }
    serde_json::json!({
        "default": g.settings.soundfont_name,
        "loaded": if g.soundfont.is_some() { g.settings.soundfont_name.clone() } else { String::new() },
        "available": available,
    })
    .to_string()
}

async fn handle_load_soundfont(req: LoadSfReq, state: &Arc<Mutex<AppState>>) -> String {
    let path = {
        let g = state.lock().await;
        // `name` and `path` are both confined to the state dir — load_soundfont
        // can only activate a SoundFont that lives in the daemon's bank.
        let name = req
            .path
            .as_deref()
            .or(req.name.as_deref())
            .unwrap_or(g.settings.soundfont_name.as_str())
            .to_string();
        match confined_path(Path::new(&g.settings.state_dir), &name) {
            Ok(pp) => pp,
            Err(e) => return json_error(&e),
        }
    };
    let p2 = path.clone();
    let result = tokio::task::spawn_blocking(move || synth::load_soundfont(&p2)).await;
    match result {
        Ok(Ok(sf)) => {
            let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let mut g = state.lock().await;
            g.soundfont = Some(sf);
            g.soundfont_path = path.clone();
            g.soundfont_error.clear();
            if let Some(n) = path.file_name().and_then(|n| n.to_str()) {
                g.settings.soundfont_name = n.to_string();
            }
            serde_json::json!({
                "loaded": true,
                "name": g.settings.soundfont_name,
                "path": path.display().to_string(),
                "bytes": bytes,
            })
            .to_string()
        }
        Ok(Err(e)) => json_error(&format!("load soundfont failed: {e}")),
        Err(e) => json_error(&format!("load task panicked: {e}")),
    }
}

async fn process_request(input: &str, state: &Arc<Mutex<AppState>>) -> String {
    let req: Request = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return json_error(&format!("invalid request: {e}")),
    };
    match req {
        Request::Render(r) => handle_render(r, state).await,
        Request::Play(r) => handle_play(r, state).await,
        Request::Stop => handle_stop(state).await,
        Request::Status => handle_status(state).await,
        Request::Soundfonts => handle_soundfonts(state).await,
        Request::LoadSoundfont(r) => handle_load_soundfont(r, state).await,
    }
}

/// Build the observable snapshot from live state + engine status. No async lock
/// is held by the `PropTree` impl — this collects, then returns owned data.
pub async fn collect_props(state: &Arc<Mutex<AppState>>) -> MusicdPropsSnapshot {
    let g = state.lock().await;
    let st = g.engine.status();
    let playing = st.is_playing();
    MusicdPropsSnapshot {
        state_dir: g.settings.state_dir.clone(),
        soundfont_name: g.settings.soundfont_name.clone(),
        soundfont_url: g.settings.soundfont_url.clone(),
        sample_rate: g.settings.sample_rate as u64,
        max_polyphony: g.settings.max_polyphony as u64,
        started_at: g.started_at_iso.clone(),
        uptime_s: g.started.elapsed().as_secs(),
        soundfont_loaded: g.soundfont.is_some(),
        soundfont_error: g.soundfont_error.clone(),
        device_available: g.device_available,
        playing,
        current_midi: if playing {
            g.current_midi.clone()
        } else {
            String::new()
        },
        position_s: st.position_s(),
        duration_s: st.duration_s(),
        renders_total: g.renders_total,
        plays_total: g.plays_total,
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

fn load_musicd_config(cli_path: Option<&Path>) -> Result<(PathBuf, MusicdSettings)> {
    if let Some(p) = cli_path {
        let cfg = cosmix_config::load_conf_mix_path::<MusicdSettings>(p)
            .with_context(|| format!("--config {}", p.display()))?;
        return Ok((p.to_path_buf(), cfg));
    }
    if let Some((path, cfg)) = load_system_config(Path::new(SYSTEM_CFG_DIR))? {
        return Ok((path, cfg));
    }
    let cfg = cosmix_config::store::load_service::<MusicdSettings>("musicd")
        .context("loading user-mode musicd config")?;
    let path = cosmix_config::store::config_dir().join("musicd.conf.mix");
    Ok((path, cfg))
}

fn load_system_config(dir: &Path) -> Result<Option<(PathBuf, MusicdSettings)>> {
    let conf_mix = dir.join("config.conf.mix");
    match std::fs::read_to_string(&conf_mix) {
        Ok(content) => {
            let cfg: MusicdSettings = cosmix_config::from_conf_mix_str(&content)
                .with_context(|| format!("parse {}", conf_mix.display()))?;
            Ok(Some((conf_mix, cfg)))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow!("read {}: {e}", conf_mix.display())),
    }
}

// ---------------------------------------------------------------------------
// Bus client loop
// ---------------------------------------------------------------------------

fn spawn_soundfont_fetch(state: Arc<Mutex<AppState>>, settings: MusicdSettings) {
    tokio::spawn(async move {
        let s = settings;
        let res = tokio::task::spawn_blocking(move || -> Result<(PathBuf, Arc<SoundFont>)> {
            let path = fetch::ensure_soundfont(&fetch::SoundfontSpec {
                state_dir: &s.state_dir,
                name: &s.soundfont_name,
                url: &s.soundfont_url,
                sha256: &s.soundfont_sha256,
            })?;
            let sf = synth::load_soundfont(&path)?;
            Ok((path, sf))
        })
        .await;
        let mut g = state.lock().await;
        match res {
            Ok(Ok((path, sf))) => {
                info!("soundfont ready: {}", path.display());
                g.soundfont = Some(sf);
                g.soundfont_path = path;
                g.soundfont_error.clear();
            }
            Ok(Err(e)) => {
                warn!("soundfont unavailable: {e}");
                g.soundfont_error = e.to_string();
            }
            Err(e) => {
                warn!("soundfont fetch task panicked: {e}");
                g.soundfont_error = format!("fetch task panicked: {e}");
            }
        }
    });
}

async fn run_bus_client_loop(state: Arc<Mutex<AppState>>, prov: cosmix_bus::RegisterProvenance) {
    let mut backoff = Duration::from_secs(1);
    loop {
        match cosmix_config::client_helpers::connect_default_with_provenance("musicd", prov.clone())
            .await
        {
            Ok(client) => {
                info!("registered as Bus service 'musicd' on broker");
                backoff = Duration::from_secs(1);
                let client = Arc::new(client);
                let world = tokio::spawn(world::run_loop(client.clone(), state.clone()));
                handle_bus_commands(client, state.clone()).await;
                world.abort();
                warn!("broker connection closed; reconnecting");
            }
            Err(e) => info!("broker unavailable; retrying in {backoff:?}: {e}"),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

async fn handle_bus_commands(client: Arc<cosmix_client::NodedClient>, state: Arc<Mutex<AppState>>) {
    let mut rx = match client.incoming_async().await {
        Some(rx) => rx,
        None => return,
    };

    while let Some(cmd) = rx.recv().await {
        // SPEC 07 §3 — props.watch returns the topic to subscribe to.
        if cmd.command == "musicd.props.watch" || cmd.command == "props.watch" {
            let body = serde_json::json!({
                "topic": world::PROPS_CHANGED_TOPIC,
                "info": "Subscribe to this topic via topic.subscribe to receive props.changed events.",
            })
            .to_string();
            if let Err(e) = client.respond(&cmd, 0, &body).await {
                error!("props.watch respond: {e}");
            }
            continue;
        }

        // SPEC 07 §2 — props.{get,list,describe} dispatch.
        if let Some(suffix) = cmd.command.strip_prefix("musicd.props.") {
            let snapshot = collect_props(&state).await;
            let args_json = props::parse_args(cmd.header("args")).or_else(|| {
                if cmd.args.is_object() && !cmd.args.as_object().unwrap().is_empty() {
                    Some(cmd.args.clone())
                } else if cmd.body.is_empty() {
                    None
                } else {
                    serde_json::from_str(&cmd.body).ok()
                }
            });
            let resp_inner =
                cosmix_props::bus::dispatch_props(&snapshot, suffix, args_json.as_ref(), true);
            let rc_u8: u8 = resp_inner.rc.clamp(0, 255) as u8;
            if let Err(e) = client.respond(&cmd, rc_u8, &resp_inner.body).await {
                error!("props respond: {e}");
            }
            continue;
        }

        // Legacy verb dispatch: strip `musicd.`, inject "action", route to handlers.
        let action = cmd.command.strip_prefix("musicd.").unwrap_or(&cmd.command);
        let request_json = if cmd.args.is_object() {
            let mut args = cmd.args.clone();
            args.as_object_mut()
                .unwrap()
                .insert("action".into(), serde_json::Value::String(action.into()));
            args.to_string()
        } else {
            serde_json::json!({ "action": action }).to_string()
        };

        let response = process_request(&request_json, &state).await;
        let rc = if response.contains("\"error\"") {
            10
        } else {
            0
        };
        if let Err(e) = client.respond(&cmd, rc, &response).await {
            error!("respond: {e}");
        }
    }

    info!("broker connection closed");
}

/// Daemon entry point — called from `main` with a tokio runtime.
pub async fn serve(config: Option<PathBuf>) -> Result<()> {
    let _log = cosmix_log::init(
        &cosmix_log::LogOpts::default(),
        &cosmix_log::StatsOpts::default(),
        cosmix_log::LogDefaults::daemon("cosmix-musicd").with_stats(false),
    );

    let (cfg_path, settings) = load_musicd_config(config.as_deref())?;
    info!("musicd config loaded from {}", cfg_path.display());

    let engine = Arc::new(PlaybackEngine::spawn());
    let device_available = play::device_available();
    if !device_available {
        warn!("no audio output device — `play` disabled, `render` available");
    }

    let soundfont_path = Path::new(&settings.state_dir).join(&settings.soundfont_name);
    let state = Arc::new(Mutex::new(AppState {
        settings: settings.clone(),
        soundfont_path,
        soundfont: None,
        soundfont_error: String::new(),
        started: Instant::now(),
        started_at_iso: cosmix_buildinfo::now_rfc3339(),
        renders_total: 0,
        plays_total: 0,
        device_available,
        current_midi: String::new(),
        engine,
    }));

    // First-run SoundFont fetch — non-blocking; the daemon serves regardless.
    spawn_soundfont_fetch(state.clone(), settings);

    // Build provenance ONCE so started_at is the true process start.
    let bi = cosmix_buildinfo::build_info!();
    let prov = cosmix_bus::RegisterProvenance::from_parts(
        bi.pkg,
        bi.version,
        bi.git_sha,
        bi.git_dirty,
        bi.build_time,
        cosmix_buildinfo::now_rfc3339(),
    );
    run_bus_client_loop(state, prov).await;
    Ok(())
}
