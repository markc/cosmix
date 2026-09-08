mod encoder;
mod stream;
mod wayland;
use cosmix_client::{BoundedIncomingEvent, SupervisedClient};
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone)]
struct Options {
    output: Option<String>,
    directory: PathBuf,
    vaapi_device: Option<PathBuf>,
}
#[derive(Default)]
struct Status {
    vaapi_device: Option<PathBuf>,
    phase: &'static str,
    path: Option<PathBuf>,
    error: Option<String>,
    frames: u64,
    fresh_frames: u64,
    acquired_frames: u64,
    dropped_frames: u64,
    repeated_frames: u64,
    refused_frames: u64,
    capture_ms: u64,
    capture_wait_ms: u64,
    capture_read_ms: u64,
    capture_normalise_ms: u64,
    encode_ms: u64,
    started: Option<Instant>,
    elapsed_ms: u64,
}
impl Status {
    fn observe_capture(&mut self, timing: wayland::FrameTiming) {
        self.capture_wait_ms += timing.wait_ms;
        self.capture_read_ms += timing.read_ms;
        self.capture_normalise_ms += timing.normalise_ms;
    }
    fn stopping(&mut self) {
        // The worker can complete after is_finished() was checked but before
        // this mutex is acquired. Terminal state must win over a late Stop.
        if matches!(self.phase, "starting" | "recording" | "screenshot") {
            self.phase = "finalising";
        }
    }
    fn value(&self) -> Value {
        let mut value = self.capture_value();
        value["encoder"] = json!(if self.vaapi_device.is_some() {
            "h264_vaapi"
        } else {
            "libx264"
        });
        value["encoder_backend"] = json!(if self.vaapi_device.is_some() {
            "vaapi"
        } else {
            "software"
        });
        value["vaapi_device"] = json!(self.vaapi_device);
        value["refused_frames"] = json!(self.refused_frames);
        value
    }
    fn capture_value(&self) -> Value {
        let build = cosmix_buildinfo::build_info!();
        let elapsed_ms = self
            .started
            .map_or(self.elapsed_ms, |start| start.elapsed().as_millis() as u64);
        let fresh_fps = if elapsed_ms == 0 {
            0.0
        } else {
            self.fresh_frames as f64 * 1000.0 / elapsed_ms as f64
        };
        json!({"recording":matches!(self.phase,"starting"|"recording"),"phase":if self.phase.is_empty(){"idle"}else{self.phase},"path":self.path,"error":self.error,"frames":self.frames,"fresh_frames":self.fresh_frames,"acquired_frames":self.acquired_frames,"dropped_frames":self.dropped_frames,"repeated_presentations":self.repeated_frames,"duplicate_frames":self.frames.saturating_sub(self.fresh_frames),"elapsed_ms":elapsed_ms,"fresh_fps":fresh_fps,"capture_ms":self.capture_ms,"capture_wait_ms":self.capture_wait_ms,"capture_read_ms":self.capture_read_ms,"capture_normalise_ms":self.capture_normalise_ms,"encode_ms":self.encode_ms,"pid":std::process::id(),"version":build.version,"git_sha":build.git_sha,"build_time":build.build_time})
    }
}
struct Job {
    cancel: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}
fn options() -> Result<Option<Options>, String> {
    let mut output = None;
    let mut vaapi_device = None;
    let mut directory = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("Videos/Cosmix"));
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--version" => {
                println!("cosmix-capture {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--help" => {
                println!(
                    "cosmix-capture [--output NAME] [--directory /absolute/path] [--vaapi-device /dev/dri/renderD128]\nNative Bus service capture: capture.screenshot, capture.start {{fps:30}}, capture.stop, capture.status.\nRecordings automatically stop at 300 seconds. Files default to ~/Videos/Cosmix.\nEncoding defaults to software libx264. An explicit VAAPI device requires hardware H.264; failures never fall back."
                );
                return Ok(None);
            }
            "--output" => output = Some(args.next().ok_or("--output requires a name")?),
            "--vaapi-device" => {
                vaapi_device = Some(vaapi_path(
                    args.next()
                        .ok_or("--vaapi-device requires an absolute device path")?,
                )?)
            }
            "--directory" => {
                directory = Some(PathBuf::from(
                    args.next().ok_or("--directory requires a path")?,
                ))
            }
            _ => return Err(format!("unknown argument {arg}")),
        }
    }
    let directory = directory.ok_or("HOME unavailable; pass --directory /absolute/path")?;
    if !directory.is_absolute() {
        return Err("capture directory must be absolute".into());
    }
    fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
    Ok(Some(Options {
        output,
        directory,
        vaapi_device,
    }))
}
fn vaapi_path(value: String) -> Result<PathBuf, String> {
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err("VAAPI device must be an absolute path".into());
    }
    Ok(path)
}
fn reserve(options: &Options, video: bool) -> Result<(PathBuf, PathBuf), String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    let final_path = options.directory.join(format!(
        "cosmix-{}-{stamp}.{}",
        std::process::id(),
        if video { "mp4" } else { "png" }
    ));
    // Names are collision checked here; publication uses a no-clobber hard
    // link, so a concurrent creator is still protected at completion.
    let partial = final_path.with_extension(if video { "partial.mp4" } else { "partial.png" });
    if final_path.exists() || partial.exists() {
        return Err("capture filename already exists".into());
    }
    Ok((final_path, partial))
}
fn publish(partial: &PathBuf, final_path: &PathBuf) -> Result<(), String> {
    fs::hard_link(partial, final_path)
        .map_err(|e| format!("publish capture without overwrite: {e}"))?;
    fs::remove_file(partial).map_err(|e| e.to_string())
}
fn run_job(
    options: Options,
    video: bool,
    fps: u32,
    path: PathBuf,
    partial: PathBuf,
    cancel: Arc<AtomicBool>,
    status: Arc<Mutex<Status>>,
) {
    let result = (|| -> Result<(), String> {
        let mut capture = wayland::Capture::connect(options.output.as_deref(), cancel.clone())?;
        let capture_started = Instant::now();
        let first = capture.frame()?;
        {
            let mut state = status.lock().unwrap();
            state.capture_ms = capture_started.elapsed().as_millis() as u64;
            state.fresh_frames = u64::from(!video);
            state.acquired_frames = 1;
            state.observe_capture(capture.timing);
        }
        if !video {
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&partial)
                .map_err(|e| e.to_string())?;
            use image::ImageEncoder;
            image::codecs::png::PngEncoder::new(file)
                .write_image(
                    &first.rgba,
                    first.width,
                    first.height,
                    image::ExtendedColorType::Rgba8,
                )
                .map_err(|e| e.to_string())?;
            publish(&partial, &path)?;
            status.lock().unwrap().frames = 1;
            return Ok(());
        }
        let mut encoder =
            encoder::Encoder::new(&partial, &first, fps, options.vaapi_device.as_deref())?;
        let depth = wayland::pipeline_depth(first.width, first.height);
        let mut previous_sequence = capture.presentation_sequence();
        let stream = stream::Stream::start(capture, fps, depth, status.clone(), cancel.clone())?;
        stream.preroll()?;
        let start = Instant::now();
        status.lock().unwrap().started = Some(start);
        let mut previous = first;
        let mut fresh = true;
        let mut frames = 0u64;
        let mut failure = None;
        status.lock().unwrap().phase = "recording";
        while start.elapsed() < Duration::from_secs(300) && !cancel.load(Ordering::Relaxed) {
            let due = Duration::from_secs_f64(frames as f64 / fps as f64);
            if start.elapsed() < due {
                std::thread::sleep(
                    due.saturating_sub(start.elapsed())
                        .min(Duration::from_millis(20)),
                );
                continue;
            }
            // A successful fresh capture admits duplication only for time
            // already elapsed; never repeat stale frames after capture failure.
            let target = recording_target(start.elapsed(), fps);
            while frames < target
                && start.elapsed() < Duration::from_secs(300)
                && !cancel.load(Ordering::Relaxed)
            {
                if let Err(error) = stream.check() {
                    failure = Some(error);
                    break;
                }
                // Preserve the initial image as slot zero. Thereafter consume
                // the FIFO in order; the preroll covers short arrival jitter.
                match if frames == 0 { Ok(None) } else { stream.next() } {
                    Ok(Some(sample)) => {
                        fresh = sample.newer_than(previous_sequence);
                        previous_sequence = previous_sequence.max(sample.sequence);
                        previous = sample.pixels;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        failure = Some(error);
                        break;
                    }
                }
                let encode_started = Instant::now();
                let encoded = encoder.write(&previous, &cancel);
                status.lock().unwrap().encode_ms += encode_started.elapsed().as_millis() as u64;
                if let Err(e) = encoded {
                    if !cancel.load(Ordering::Relaxed) {
                        failure = Some(e);
                    }
                    break;
                }
                frames += 1;
                let mut state = status.lock().unwrap();
                state.frames = frames;
                if fresh {
                    state.fresh_frames += 1;
                    fresh = false;
                }
            }
            if failure.is_some() {
                break;
            }
        }
        // A final in-flight write can finish just beyond the automatic stop.
        // The admitted recording budget remains exactly five minutes.
        let elapsed = start.elapsed().min(Duration::from_secs(300));
        {
            let mut state = status.lock().unwrap();
            state.phase = "finalising";
            state.started = None;
            state.elapsed_ms = elapsed.as_millis() as u64;
        }
        if failure.is_none() && recording_shortfall(elapsed, fps, frames) > 2 {
            failure = Some(format!(
                "encoder fell behind: {frames} frames represent {:.3}s of {:.3}s recording; MP4 is shorter than wall time",
                frames as f64 / f64::from(fps),
                elapsed.as_secs_f64()
            ));
        }
        drop(stream);
        encoder.finish()?;
        if frames == 0 {
            return Err("recording stopped before first frame".into());
        }
        publish(&partial, &path)?;
        if let Some(e) = failure {
            return Err(e);
        }
        Ok(())
    })();
    let mut state = status.lock().unwrap();
    if let Some(started) = state.started.take() {
        state.elapsed_ms = started.elapsed().as_millis() as u64;
    }
    match result {
        Ok(()) => {
            state.phase = "complete";
            state.error = None;
        }
        Err(error) => {
            state.phase = "failed";
            state.error = Some(error);
        }
    }
}
fn recording_target(elapsed: Duration, fps: u32) -> u64 {
    ((elapsed.as_secs_f64() * fps as f64).floor() as u64)
        .saturating_add(1)
        .min(u64::from(fps) * 300)
}

fn recording_shortfall(elapsed: Duration, fps: u32, frames: u64) -> u64 {
    ((elapsed.as_secs_f64() * f64::from(fps)).round() as u64).saturating_sub(frames)
}

fn request(command: &str, body: &str) -> Result<Option<(bool, u32)>, String> {
    if body.len() > 1024 {
        return Err("capture request exceeds 1024 bytes".into());
    }
    let value: Value = if body.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(body).map_err(|e| e.to_string())?
    };
    let object = value.as_object().ok_or("request body must be an object")?;
    match command {
        "capture.start" => {
            if object.keys().any(|k| k != "fps") {
                return Err("unknown recording option".into());
            }
            let fps = match object.get("fps") {
                Some(v) => v
                    .as_u64()
                    .filter(|v| (1..=60).contains(v))
                    .ok_or("fps must be an integer 1..60")? as u32,
                None => 30,
            };
            Ok(Some((true, fps)))
        }
        "capture.screenshot" | "capture.stop" | "capture.status" => {
            if !object.is_empty() {
                return Err("this command takes an empty object".into());
            }
            Ok((command == "capture.screenshot").then_some((false, 30)))
        }
        _ => Err("unknown capture command".into()),
    }
}
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), String> {
    let Some(options) = options()? else {
        return Ok(());
    };
    let client = SupervisedClient::connect_options(
        "capture",
        &cosmix_config::client_helpers::resolve_noded_url(),
    )
    .bounded_incoming(16)
    .connect()
    .await
    .map_err(|e| e.to_string())?;
    let mut incoming = client.incoming_bounded().ok_or("no incoming Bus queue")?;
    let status = Arc::new(Mutex::new(Status {
        vaapi_device: options.vaapi_device.clone(),
        ..Default::default()
    }));
    let mut job: Option<Job> = None;
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| e.to_string())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .map_err(|e| e.to_string())?;
    loop {
        tokio::select! {
            _ = term.recv() => break,
            _ = interrupt.recv() => break,
            command = incoming.recv() => {
                let Some(event) = command else { break };
                let command = match event {
                    BoundedIncomingEvent::Command(command) => command,
                    BoundedIncomingEvent::Overflow { .. } => continue,
                };
                if job.as_ref().is_some_and(|j|j.thread.is_finished()) { let _ = job.take().unwrap().thread.join(); }
                let result = (|| -> Result<Value,String> {
                    let start = request(&command.command,&command.body)?;
                    if let Some((video,fps)) = start {
                        if job.is_some() { return Err("capture job already active".into()); }
                        let (path,partial) = reserve(&options,video)?;
                        *status.lock().unwrap() = Status {phase:if video {"starting"}else{"screenshot"},path:Some(path.clone()),vaapi_device:options.vaapi_device.clone(),..Default::default()};
                        let cancel = Arc::new(AtomicBool::new(false));
                        let (opts,flag,state) = (options.clone(),cancel.clone(),status.clone());
                        let thread = std::thread::Builder::new().name("cosmix-capture".into()).spawn(move ||run_job(opts,video,fps,path,partial,flag,state)).map_err(|e|e.to_string())?;
                        job = Some(Job {cancel,thread});
                    } else if command.command=="capture.stop" && let Some(job)=&job {
                        job.cancel.store(true,Ordering::Relaxed); status.lock().unwrap().stopping();
                    }
                    Ok(status.lock().unwrap().value())
                })();
                let (rc,body) = match result {Ok(v)=>(0,v),Err(e)=>(10,json!({"error":e}))};
                let _ = tokio::time::timeout(Duration::from_secs(2),client.respond(&command,rc,&body.to_string())).await;
            }
        }
    }
    if let Some(job) = job {
        job.cancel.store(true, Ordering::Relaxed);
        let _ = job.thread.join();
    }
    client.close().await;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn vaapi_is_explicit_and_status_identifies_selected_encoder() {
        assert!(vaapi_path("renderD128".into()).is_err());
        assert!(vaapi_path("".into()).is_err());
        let path = vaapi_path("/dev/dri/renderD128".into()).unwrap();
        let software = Status::default().value();
        assert_eq!(software["encoder"], "libx264");
        let hardware = Status {
            vaapi_device: Some(path),
            ..Default::default()
        }
        .value();
        assert_eq!(hardware["encoder"], "h264_vaapi");
        assert_eq!(hardware["encoder_backend"], "vaapi");
        assert_eq!(hardware["vaapi_device"], "/dev/dri/renderD128");
    }
    #[test]
    fn stop_after_worker_completion_preserves_terminal_state() {
        for phase in ["complete", "failed"] {
            let mut state = Status {
                phase,
                ..Default::default()
            };
            state.stopping();
            assert_eq!(state.phase, phase);
        }
        for phase in ["starting", "recording", "screenshot"] {
            let mut state = Status {
                phase,
                ..Default::default()
            };
            state.stopping();
            assert_eq!(state.phase, "finalising");
        }
    }
    #[test]
    fn slow_capture_cannot_extend_recording_frame_budget() {
        assert_eq!(recording_target(Duration::ZERO, 30), 1);
        assert_eq!(recording_target(Duration::from_secs(300), 30), 9000);
        assert_eq!(recording_target(Duration::from_secs(302), 60), 18000);
    }
    #[test]
    fn finalisation_detects_a_short_movie_instead_of_claiming_realtime() {
        assert_eq!(recording_shortfall(Duration::from_secs(10), 30, 300), 0);
        assert_eq!(recording_shortfall(Duration::from_secs(10), 30, 299), 1);
        assert_eq!(recording_shortfall(Duration::from_secs(10), 30, 240), 60);
    }
    #[test]
    fn strict_commands_and_fps() {
        assert!(request("capture.status", &" ".repeat(1025)).is_err());
        assert_eq!(request("capture.start", "{}").unwrap(), Some((true, 30)));
        for body in [
            "{\"fps\":0}",
            "{\"fps\":61}",
            "{\"fps\":1.5}",
            "{\"path\":\"/tmp/a\"}",
            "[]",
        ] {
            assert!(request("capture.start", body).is_err());
        }
        assert!(request("capture.stop", "{\"fps\":30}").is_err());
        assert!(request("other", "{}").is_err());
    }
    #[test]
    fn publication_never_clobbers_existing_media() {
        let dir = tempfile::tempdir().unwrap();
        let partial = dir.path().join("a.partial.png");
        let final_path = dir.path().join("a.png");
        fs::write(&partial, b"new").unwrap();
        fs::write(&final_path, b"keep").unwrap();
        assert!(publish(&partial, &final_path).is_err());
        assert_eq!(fs::read(final_path).unwrap(), b"keep");
    }
}
