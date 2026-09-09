//! GStreamer owns demux, decode and the audio/video clock on a worker thread.
//! The renderer takes at most one latest frame. No decode or seek runs in Bevy.
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::AppSink;
use gstreamer_video::VideoFrameExt;
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Serialize)]
pub struct Status {
    pub phase: String,
    pub path: Option<PathBuf>,
    pub position: f64,
    pub duration: f64,
    pub volume: f64,
    pub muted: bool,
    pub fullscreen: bool,
    pub width: u32,
    pub height: u32,
    pub decoded_frames: u64,
    pub replaced_frames: u64,
    pub generation: u64,
    pub error: Option<String>,
    pub backend: &'static str,
}
impl Default for Status {
    fn default() -> Self {
        Self {
            phase: "idle".into(),
            path: None,
            position: 0.0,
            duration: 0.0,
            volume: 0.8,
            muted: false,
            fullscreen: false,
            width: 0,
            height: 0,
            decoded_frames: 0,
            replaced_frames: 0,
            generation: 0,
            error: None,
            backend: "gstreamer-rgba",
        }
    }
}
impl Status {
    pub fn value(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap();
        v["version"] = json!(env!("CARGO_PKG_VERSION"));
        v["pid"] = json!(std::process::id());
        v
    }
}
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}
#[derive(Default)]
pub struct Shared {
    pub status: Status,
    pub frame: Option<Frame>,
}
#[derive(Clone, Debug)]
pub enum Action {
    Open(PathBuf),
    Play,
    Pause,
    Toggle,
    Stop,
    Seek(f64),
    Relative(f64),
    Volume(f64),
    Mute(bool),
    Fullscreen(bool),
    ToggleFullscreen,
    Quit,
}
pub struct Request {
    pub action: Action,
    pub reply: Option<tokio::sync::oneshot::Sender<Result<Value, String>>>,
}
pub struct Player {
    pub sender: mpsc::SyncSender<Request>,
    pub shared: Arc<Mutex<Shared>>,
    pub quit: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Player {
    pub fn start() -> Self {
        let (sender, receiver) = mpsc::sync_channel(32);
        let shared = Arc::new(Mutex::new(Shared::default()));
        let quit = Arc::new(AtomicBool::new(false));
        let (state, stop) = (shared.clone(), quit.clone());
        let worker = thread::Builder::new()
            .name("media-decode".into())
            .spawn(move || {
                if let Err(error) = run(receiver, &state, &stop) {
                    state.lock().unwrap().status.error = Some(error);
                    state.lock().unwrap().status.phase = "failed".into();
                }
            })
            .expect("media worker");
        Self {
            sender,
            shared,
            quit,
            worker: Some(worker),
        }
    }
    pub fn send(&self, action: Action) {
        if self
            .sender
            .try_send(Request {
                action,
                reply: None,
            })
            .is_err()
        {
            self.shared.lock().unwrap().status.error =
                Some("playback command queue unavailable".into());
        }
    }
}
impl Drop for Player {
    fn drop(&mut self) {
        self.quit.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub fn parse(command: &str, body: &str) -> Result<Action, String> {
    if body.len() > 8192 {
        return Err("request exceeds 8192 bytes".into());
    }
    let value: Value = if body.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(body).map_err(|e| e.to_string())?
    };
    let map = value.as_object().ok_or("expected an object")?;
    let field = match command {
        "media.open" => Some("path"),
        "media.seek" => Some("seconds"),
        "media.volume" => Some("value"),
        "media.mute" | "media.fullscreen" => Some("value"),
        _ => None,
    };
    if map.keys().any(|k| Some(k.as_str()) != field) {
        return Err("unknown argument".into());
    }
    let number = |key: &str| {
        value[key]
            .as_f64()
            .filter(|v| v.is_finite())
            .ok_or_else(|| format!("{key} must be finite"))
    };
    Ok(match command {
        "media.open" => {
            let path = value["path"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or("path required")?;
            let path = PathBuf::from(path);
            if !path.is_absolute() {
                return Err("Bus paths must be absolute".into());
            }
            Action::Open(path)
        }
        "media.play" => Action::Play,
        "media.pause" => Action::Pause,
        "media.toggle" => Action::Toggle,
        "media.stop" => Action::Stop,
        "media.seek" => {
            let s = number("seconds")?;
            if !(0.0..=604800.0).contains(&s) {
                return Err("seconds outside 0..604800".into());
            }
            Action::Seek(s)
        }
        "media.volume" => {
            let v = number("value")?;
            if !(0.0..=1.0).contains(&v) {
                return Err("volume outside 0..1".into());
            }
            Action::Volume(v)
        }
        "media.mute" => Action::Mute(value["value"].as_bool().ok_or("value must be boolean")?),
        "media.fullscreen" => {
            Action::Fullscreen(value["value"].as_bool().ok_or("value must be boolean")?)
        }
        "media.fullscreen.toggle" => Action::ToggleFullscreen,
        "media.quit" => Action::Quit,
        _ => return Err("unknown media command".into()),
    })
}

fn run(
    receiver: mpsc::Receiver<Request>,
    shared: &Arc<Mutex<Shared>>,
    quit: &Arc<AtomicBool>,
) -> Result<(), String> {
    gst::init().map_err(|e| e.to_string())?;
    let sink = AppSink::builder()
        .caps(
            &gst::Caps::builder("video/x-raw")
                .field("format", "RGBA")
                .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
                .build(),
        )
        .max_buffers(2)
        .drop(true)
        .sync(true)
        .build();
    let audio = gst::ElementFactory::make("pulsesink")
        .build()
        .map_err(|e| format!("PipeWire/Pulse sink unavailable: {e}"))?;
    if let Ok(server) = std::env::var("PULSE_SERVER") {
        audio.set_property("server", server.strip_prefix("unix:").unwrap_or(&server));
    }
    let play = gst::ElementFactory::make("playbin")
        .build()
        .map_err(|e| e.to_string())?;
    play.set_property("video-sink", &sink);
    play.set_property("audio-sink", &audio);
    play.set_property("volume", 0.8f64);
    let bus = play.bus().ok_or("playbin has no bus")?;
    let mut last = Instant::now();
    while !quit.load(Ordering::Relaxed) {
        match receiver.recv_timeout(Duration::from_millis(5)) {
            Ok(request) => {
                let result = apply(&play, request.action, shared, quit);
                if let Err(ref error) = result {
                    shared.lock().unwrap().status.error = Some(error.clone());
                }
                if let Some(reply) = request.reply {
                    let _ = reply.send(result.map(|()| shared.lock().unwrap().status.value()));
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        for message in bus.iter_timed(gst::ClockTime::ZERO).take(32) {
            match message.view() {
                gst::MessageView::Error(error) => {
                    let _ = play.set_state(gst::State::Null);
                    let mut s = shared.lock().unwrap();
                    s.status.phase = "failed".into();
                    s.status.error = Some(error.error().to_string());
                }
                gst::MessageView::Eos(_) => {
                    let _ = play.set_state(gst::State::Paused);
                    shared.lock().unwrap().status.phase = "ended".into();
                }
                gst::MessageView::ClockLost(_)
                    if shared.lock().unwrap().status.phase == "playing" =>
                {
                    recover_clock(&play)?;
                }
                _ => {}
            }
        }
        if let Some(sample) = sink
            .try_pull_sample(gst::ClockTime::ZERO)
            .or_else(|| sink.try_pull_preroll(gst::ClockTime::ZERO))
        {
            let result = sample_frame(&sample);
            let mut s = shared.lock().unwrap();
            match result {
                Ok(frame) => {
                    s.status.width = frame.width;
                    s.status.height = frame.height;
                    s.status.decoded_frames += 1;
                    if s.frame.is_some() {
                        s.status.replaced_frames += 1;
                    }
                    s.frame = Some(frame);
                }
                Err(error) => {
                    s.status.error = Some(error);
                    s.status.phase = "failed".into();
                    drop(s);
                    let _ = play.set_state(gst::State::Null);
                }
            }
        }
        if last.elapsed() >= Duration::from_millis(100) {
            let position = play.query_position::<gst::ClockTime>();
            let duration = play.query_duration::<gst::ClockTime>();
            let mut s = shared.lock().unwrap();
            if let Some(p) = position {
                s.status.position = p.nseconds() as f64 / 1e9;
            }
            if let Some(d) = duration {
                s.status.duration = d.nseconds() as f64 / 1e9;
            }
            last = Instant::now();
        }
    }
    play.set_state(gst::State::Null)
        .map_err(|e| e.to_string())?;
    Ok(())
}
fn apply(
    play: &gst::Element,
    action: Action,
    shared: &Arc<Mutex<Shared>>,
    quit: &AtomicBool,
) -> Result<(), String> {
    let state = |s| play.set_state(s).map(|_| ()).map_err(|e| e.to_string());
    match action {
        Action::Open(path) => {
            let path = path.canonicalize().map_err(|e| e.to_string())?;
            if !path.is_file() {
                return Err("not a regular media file".into());
            }
            let uri = gst::glib::filename_to_uri(&path, None).map_err(|e| e.to_string())?;
            state(gst::State::Null)?;
            {
                let mut s = shared.lock().unwrap();
                let generation = s.status.generation + 1;
                let (volume, muted) = (s.status.volume, s.status.muted);
                let fullscreen = s.status.fullscreen;
                s.frame = None;
                s.status = Status {
                    path: Some(path),
                    phase: "playing".into(),
                    generation,
                    volume,
                    muted,
                    fullscreen,
                    ..Default::default()
                };
            }
            play.set_property("uri", uri.as_str());
            state(gst::State::Playing)?;
        }
        Action::Quit => quit.store(true, Ordering::Relaxed),
        Action::Volume(v) => {
            play.set_property("volume", v);
            shared.lock().unwrap().status.volume = v;
        }
        Action::Mute(v) => {
            play.set_property("mute", v);
            shared.lock().unwrap().status.muted = v;
        }
        Action::Fullscreen(v) => shared.lock().unwrap().status.fullscreen = v,
        Action::ToggleFullscreen => {
            let mut s = shared.lock().unwrap();
            s.status.fullscreen = !s.status.fullscreen;
        }
        other => {
            let snapshot = shared.lock().unwrap().status.clone();
            if snapshot.path.is_none() {
                return Err("open a media file first".into());
            }
            match other {
                Action::Play | Action::Toggle
                    if !matches!(other, Action::Toggle) || snapshot.phase != "playing" =>
                {
                    if snapshot.phase == "ended" {
                        play.seek_simple(gst::SeekFlags::FLUSH, gst::ClockTime::ZERO)
                            .map_err(|e| e.to_string())?;
                    }
                    state(gst::State::Playing)?;
                    shared.lock().unwrap().status.phase = "playing".into();
                }
                Action::Pause | Action::Toggle => {
                    // Keep EOS visible so the next play rewinds the pipeline.
                    if snapshot.phase == "ended" {
                        return Ok(());
                    }
                    state(gst::State::Paused)?;
                    shared.lock().unwrap().status.phase = "paused".into();
                }
                Action::Stop => {
                    state(gst::State::Ready)?;
                    let mut s = shared.lock().unwrap();
                    s.status.phase = "stopped".into();
                    s.status.position = 0.0;
                    s.status.generation += 1;
                    s.frame = None;
                }
                Action::Seek(seconds) | Action::Relative(seconds) => {
                    let relative = matches!(other, Action::Relative(_));
                    let seconds = if relative {
                        snapshot.position + seconds
                    } else {
                        seconds
                    };
                    let seconds = seconds.max(0.0).min(if snapshot.duration > 0.0 {
                        snapshot.duration
                    } else {
                        604800.0
                    });
                    if snapshot.phase == "stopped" {
                        state(gst::State::Paused)?;
                        // A READY pipeline has not discovered its streams yet.
                        // Wait on this worker, never on Bevy's render thread.
                        let (result, current, _) = play.state(gst::ClockTime::from_seconds(3));
                        result.map_err(|e| e.to_string())?;
                        if current != gst::State::Paused {
                            return Err("media is still preparing; retry seek".into());
                        }
                    }
                    play.seek_simple(
                        gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                        gst::ClockTime::from_nseconds((seconds * 1e9) as u64),
                    )
                    .map_err(|e| e.to_string())?;
                    let mut s = shared.lock().unwrap();
                    s.frame = None;
                    s.status.generation += 1;
                    s.status.position = seconds;
                    if snapshot.phase == "ended" || snapshot.phase == "stopped" {
                        s.status.phase = "paused".into();
                    }
                }
                _ => unreachable!(),
            }
        }
    }
    Ok(())
}
fn recover_clock(play: &gst::Element) -> Result<(), String> {
    play.set_state(gst::State::Paused)
        .map_err(|e| e.to_string())?;
    play.set_state(gst::State::Playing)
        .map_err(|e| e.to_string())?;
    Ok(())
}
fn sample_frame(sample: &gst::Sample) -> Result<Frame, String> {
    let info = gstreamer_video::VideoInfo::from_caps(sample.caps().ok_or("missing video caps")?)
        .map_err(|e| e.to_string())?;
    let width = info.width();
    let height = info.height();
    if width == 0 || height == 0 || width > 4096 || height > 4096 {
        return Err("video exceeds supported 4096x4096 dimensions".into());
    }
    let video = gstreamer_video::VideoFrameRef::from_buffer_ref_readable(
        sample.buffer().ok_or("missing video buffer")?,
        &info,
    )
    .map_err(|e| e.to_string())?;
    let stride = usize::try_from(video.plane_stride()[0]).map_err(|_| "negative video stride")?;
    let pixels = pack_rgba(
        video.plane_data(0).map_err(|e| e.to_string())?,
        stride,
        width,
        height,
    )?;
    Ok(Frame {
        width,
        height,
        pixels,
    })
}
fn pack_rgba(data: &[u8], stride: usize, width: u32, height: u32) -> Result<Vec<u8>, String> {
    let row = width as usize * 4;
    if width == 0 || height == 0 || width > 4096 || height > 4096 || stride < row {
        return Err("invalid video layout".into());
    }
    let required = (height as usize - 1)
        .checked_mul(stride)
        .and_then(|v| v.checked_add(row))
        .ok_or("video layout overflow")?;
    if data.len() < required {
        return Err("truncated video buffer".into());
    }
    let mut pixels = Vec::with_capacity(row * height as usize);
    for y in 0..height as usize {
        pixels.extend_from_slice(&data[y * stride..y * stride + row]);
    }
    Ok(pixels)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pause_preserves_eos_for_replay() {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();
        let shared = Arc::new(Mutex::new(Shared::default()));
        {
            let mut s = shared.lock().unwrap();
            s.status.path = Some(PathBuf::from("/test.mp3"));
            s.status.phase = "ended".into();
        }
        apply(
            pipeline.upcast_ref(),
            Action::Pause,
            &shared,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(shared.lock().unwrap().status.phase, "ended");
    }
    #[test]
    fn clock_recovery_returns_pipeline_to_playing() {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();
        pipeline.set_state(gst::State::Playing).unwrap();
        recover_clock(pipeline.upcast_ref()).unwrap();
        assert_eq!(pipeline.current_state(), gst::State::Playing);
        pipeline.set_state(gst::State::Null).unwrap();
    }
    #[test]
    fn padded_rows_are_copied_without_padding() {
        assert_eq!(
            pack_rgba(&[1, 2, 3, 4, 99, 99, 5, 6, 7, 8], 6, 1, 2).unwrap(),
            [1, 2, 3, 4, 5, 6, 7, 8]
        );
        assert!(pack_rgba(&[0; 7], 4, 1, 2).is_err());
        assert!(pack_rgba(&[], usize::MAX, 1, 3).is_err());
    }
    #[test]
    fn commands_reject_invalid_ranges_and_extra_fields() {
        for body in [
            "{\"value\":2}",
            "{\"value\":-1}",
            "{\"value\":0.5,\"extra\":true}",
            "[]",
        ] {
            assert!(parse("media.volume", body).is_err());
        }
        assert!(parse("media.open", "{\"path\":\"relative.mp4\"}").is_err());
        assert!(parse("media.pause", "{\"path\":\"/tmp/a\"}").is_err());
        assert!(parse("media.seek", "{\"seconds\":-1}").is_err());
        assert!(parse("media.mute", "{\"value\":\"true\"}").is_err());
        assert!(parse("media.fullscreen.toggle", "{\"value\":true}").is_err());
    }
    #[test]
    fn queued_fullscreen_toggles_use_worker_state() {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();
        let shared = Arc::new(Mutex::new(Shared::default()));
        let quit = AtomicBool::new(false);
        for expected in [true, false] {
            apply(
                pipeline.upcast_ref(),
                parse("media.fullscreen.toggle", "{}").unwrap(),
                &shared,
                &quit,
            )
            .unwrap();
            assert_eq!(shared.lock().unwrap().status.fullscreen, expected);
        }
    }
}
