mod blob;
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
    region: Option<wayland::Region>,
    directory: PathBuf,
    vaapi_device: Option<PathBuf>,
}
#[derive(Default)]
struct Status {
    vaapi_device: Option<PathBuf>,
    phase: &'static str,
    path: Option<PathBuf>,
    error: Option<String>,
    blob: Option<Value>,
    blob_error: Option<String>,
    /// True from publication until the dual-write lands `blob` or
    /// `blob_error` — disambiguates `blob: null` on a terminal phase
    /// (in flight) from a capture that never published (never
    /// coming). Not in the JSON directly; `capture_value` surfaces it.
    blob_pending: bool,
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
    /// Which job owns this status. A new capture resets the status
    /// under a fresh generation; a still-running dual-write from an
    /// older job lands only on a matching generation, so a stale
    /// upload never writes into the new job's status.
    generation: u64,
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
    /// Land a finished dual-write. A newer job has since reset the
    /// status under its own generation, so a stale uploader's result
    /// is refused, not written into the new job's status; the caller
    /// logs the drop. Returns whether the result landed.
    fn land_upload(&mut self, generation: u64, outcome: Result<Value, String>) -> bool {
        if self.generation != generation {
            return false;
        }
        self.blob_pending = false;
        match outcome {
            Ok(reference) => self.blob = Some(reference),
            Err(error) => self.blob_error = Some(error),
        }
        true
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
        json!({"recording":matches!(self.phase,"starting"|"recording"),"phase":if self.phase.is_empty(){"idle"}else{self.phase},"path":self.path,"error":self.error,"blob":self.blob,"blob_error":self.blob_error,"blob_pending":self.blob_pending,"frames":self.frames,"fresh_frames":self.fresh_frames,"acquired_frames":self.acquired_frames,"dropped_frames":self.dropped_frames,"repeated_presentations":self.repeated_frames,"duplicate_frames":self.frames.saturating_sub(self.fresh_frames),"elapsed_ms":elapsed_ms,"fresh_fps":fresh_fps,"capture_ms":self.capture_ms,"capture_wait_ms":self.capture_wait_ms,"capture_read_ms":self.capture_read_ms,"capture_normalise_ms":self.capture_normalise_ms,"encode_ms":self.encode_ms,"pid":std::process::id(),"version":build.version,"git_sha":build.git_sha,"build_time":build.build_time})
    }
}
struct Job {
    cancel: Arc<AtomicBool>,
    /// Set by the worker once the terminal phase is written: the
    /// capture itself is over and only the detached dual-write may
    /// still run, so admission no longer belongs to this job.
    settled: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

/// Admission view of the job slot: empty, or held by a worker that
/// already wrote its terminal phase. A settled worker only has the
/// detached dual-write left, so the next capture may start while that
/// upload is still in flight.
fn slot_free(job: &Option<Job>) -> bool {
    job.as_ref().is_none_or(|job| job.settled.load(Ordering::Relaxed))
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
        region: None,
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
#[allow(clippy::too_many_arguments)]
fn run_job(
    options: Options,
    video: bool,
    fps: u32,
    path: PathBuf,
    partial: PathBuf,
    cancel: Arc<AtomicBool>,
    status: Arc<Mutex<Status>>,
    generation: u64,
    settled: Arc<AtomicBool>,
    lane: blob::Lane,
    shutdown: Arc<AtomicBool>,
) {
    // Whether the finished file landed at `path` — publish succeeded,
    // so the blob-store copy is owed even when encoding then reports
    // a duration failure (the MP4 is still there and usable).
    let mut published = false;
    let result = (|| -> Result<(), String> {
        let mut capture = wayland::Capture::connect(options.output.as_deref(), cancel.clone())?;
        capture.region = options.region;
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
            published = true;
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
        published = true;
        if let Some(e) = failure {
            return Err(e);
        }
        Ok(())
    })();
    let upload = (published && !shutdown.load(Ordering::Relaxed)).then(|| {
        let exit = shutdown.clone();
        move || lane.bind().and_then(|bind| blob::upload(&bind, &path, exit))
    });
    settle_job(&status, generation, &settled, upload, result);
}

/// run_job's terminal wiring, split off so the dual-write rules are
/// testable without Wayland or a Bus: write the terminal phase, mark
/// the job settled — admission may start the next capture while the
/// dual-write still runs — then upload with the status lock dropped.
/// The result lands only on a matching generation, so a stale upload
/// never writes into a newer job's status. `upload` is None when
/// nothing was published or process shutdown is abandoning the copy
/// (`capture.stop` deliberately does not count: a stopped recording
/// still owes its dual-write).
fn settle_job(
    status: &Arc<Mutex<Status>>,
    generation: u64,
    settled: &AtomicBool,
    upload: Option<impl FnOnce() -> Result<Value, String>>,
    result: Result<(), String>,
) {
    {
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
        state.blob_pending = upload.is_some();
        settled.store(true, Ordering::Relaxed);
    }
    // Dual-write into the blob store: the file is the truth and is
    // already published, so this copy is additive — its failure
    // records `blob_error` and never demotes the terminal phase. The
    // lock is dropped across the upload (a five-minute MP4 takes
    // minutes): status meanwhile shows the terminal phase with
    // `blob: null`, then the reference.
    if let Some(upload) = upload {
        let outcome = upload();
        let mut state = status.lock().unwrap();
        if !state.land_upload(generation, outcome) {
            eprintln!(
                "capture: dual-write from job {generation} dropped — a newer job owns the status"
            );
        }
    }
}
/// Stop the active job for process shutdown and wait for its worker
/// without ever joining inside the runtime: on this current-thread
/// runtime only the main thread drives the IO and timers the worker's
/// `block_on` parks on, so a synchronous join deadlocks (worker waits
/// for the timer that only main's loop would fire; main waits for the
/// join). The blocking pool owns the join, bounded — a worker that
/// will not stop in `bound` is abandoned, not waited out. Returns
/// whether the worker finished within the bound.
async fn reap(job: Job, shutdown: &AtomicBool, bound: Duration) -> bool {
    job.cancel.store(true, Ordering::Relaxed);
    shutdown.store(true, Ordering::Relaxed);
    let thread = job.thread;
    let joined = tokio::task::spawn_blocking(move || thread.join().is_ok());
    tokio::time::timeout(bound, joined).await.is_ok_and(|joined| joined.is_ok())
}

fn recording_target(elapsed: Duration, fps: u32) -> u64 {
    ((elapsed.as_secs_f64() * fps as f64).floor() as u64)
        .saturating_add(1)
        .min(u64::from(fps) * 300)
}

fn recording_shortfall(elapsed: Duration, fps: u32, frames: u64) -> u64 {
    ((elapsed.as_secs_f64() * f64::from(fps)).round() as u64).saturating_sub(frames)
}

#[derive(Debug, PartialEq)]
struct JobRequest {
    video: bool,
    fps: u32,
    output: Option<String>,
    region: Option<wayland::Region>,
}

fn request(command: &str, body: &str) -> Result<Option<JobRequest>, String> {
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
            Ok(Some(JobRequest {
                video: true,
                fps,
                output: None,
                region: None,
            }))
        }
        "capture.screenshot" => {
            if object.keys().any(|k| k != "output" && k != "region") {
                return Err("unknown screenshot option".into());
            }
            let output = match object.get("output") {
                None => None,
                Some(Value::String(name)) if !name.is_empty() => Some(name.clone()),
                _ => return Err("output must be a non-empty string".into()),
            };
            let region = object
                .get("region")
                .map(wayland::Region::parse)
                .transpose()?;
            Ok(Some(JobRequest {
                video: false,
                fps: 30,
                output,
                region,
            }))
        }
        "capture.stop" | "capture.status" => {
            if !object.is_empty() {
                return Err("this command takes an empty object".into());
            }
            Ok(None)
        }
        _ => Err("unknown capture command".into()),
    }
}
fn main() -> Result<(), String> {
    // --version/-V first, before the tokio runtime exists: a thread- or
    // fd-starved host must still get an answer, not a runtime-build panic.
    // `leading`: capture's option values are free strings with no `--`
    // escape (`--output --version` names an output), so only argv[1] asks.
    cosmix_buildinfo::exit_on_version!(leading);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build the tokio runtime")
        .block_on(async_main())
}

async fn async_main() -> Result<(), String> {
    let Some(options) = options()? else {
        return Ok(());
    };
    let client = Arc::new(
        SupervisedClient::connect_options(
            "capture",
            &cosmix_config::client_helpers::resolve_noded_url(),
        )
        .bounded_incoming(16)
        .connect()
        .await
        .map_err(|e| e.to_string())?,
    );
    let mut incoming = client.incoming_bounded().ok_or("no incoming Bus queue")?;
    let status = Arc::new(Mutex::new(Status {
        vaapi_device: options.vaapi_device.clone(),
        ..Default::default()
    }));
    // The worker thread dual-writes through this handle: the one
    // lane-resolution props call parks the worker via the runtime
    // handle, never the Bus loop below.
    let lane = blob::Lane::new(client.clone(), tokio::runtime::Handle::current());
    let mut job: Option<Job> = None;
    // Set only on the process-exit paths (SIGTERM, SIGINT, Bus loss)
    // — never by `capture.stop`, whose recordings still owe their
    // dual-write. Abandons lane resolution and an in-flight upload.
    let shutdown = Arc::new(AtomicBool::new(false));
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
                if job.is_some() && slot_free(&job) {
                    // Settled: terminal phase written, only the detached
                    // dual-write may still run. Free admission and drop the
                    // handle — the uploader finishes on its own and its
                    // result lands on a matching generation only.
                    drop(job.take());
                }
                let result = (|| -> Result<Value,String> {
                    let start = request(&command.command,&command.body)?;
                    if let Some(JobRequest {video,fps,output,region}) = start {
                        if job.is_some() { return Err("capture job already active".into()); }
                        let (path,partial) = reserve(&options,video)?;
                        let generation = status.lock().unwrap().generation+1;
                        *status.lock().unwrap() = Status {phase:if video {"starting"}else{"screenshot"},path:Some(path.clone()),vaapi_device:options.vaapi_device.clone(),generation,..Default::default()};
                        let cancel = Arc::new(AtomicBool::new(false));
                        let settled = Arc::new(AtomicBool::new(false));
                        let (mut opts,flag,state,done,job_lane,exit) = (options.clone(),cancel.clone(),status.clone(),settled.clone(),lane.clone(),shutdown.clone());
                        if output.is_some() { opts.output=output; }
                        opts.region=region;
                        let thread = std::thread::Builder::new().name("cosmix-capture".into()).spawn(move ||run_job(opts,video,fps,path,partial,flag,state,generation,done,job_lane,exit)).map_err(|e|e.to_string())?;
                        job = Some(Job {cancel,settled,thread});
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
    // Both exit paths — signal and Bus loss — land here; neither may
    // join the worker synchronously inside the runtime (see `reap`).
    if let Some(job) = job {
        let _ = reap(job, &shutdown, Duration::from_secs(5)).await;
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
        assert_eq!(
            request("capture.start", "{}").unwrap(),
            Some(JobRequest {
                video: true,
                fps: 30,
                output: None,
                region: None
            })
        );
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
    fn screenshot_region_arguments_and_no_arg_compatibility() {
        let empty = JobRequest {
            video: false,
            fps: 30,
            output: None,
            region: None,
        };
        assert_eq!(request("capture.screenshot", "{}").unwrap(), Some(empty));
        assert_eq!(
            request("capture.screenshot", "").unwrap(),
            request("capture.screenshot", "{}").unwrap()
        );
        let job = request(
            "capture.screenshot",
            r#"{"output":"Output-1","region":{"x":10,"y":20,"width":30,"height":40}}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(job.output.as_deref(), Some("Output-1"));
        assert_eq!(
            job.region,
            Some(wayland::Region {
                x: 10,
                y: 20,
                width: 30,
                height: 40
            })
        );
        for body in [
            r#"{"output":""}"#,
            r#"{"output":null}"#,
            r#"{"region":null}"#,
            r#"{"region":{"x":-1,"y":0,"width":1,"height":1}}"#,
            r#"{"region":{"x":0,"y":0,"width":0,"height":1}}"#,
            r#"{"region":{"x":0.5,"y":0,"width":1,"height":1}}"#,
            r#"{"region":{"x":2147483647,"y":0,"width":1,"height":1}}"#,
            r#"{"region":{"x":0,"y":0,"width":1,"height":1,"extra":0}}"#,
            r#"{"region":{"x":0,"y":0,"width":1}}"#,
            r#"{"fps":30}"#,
        ] {
            assert!(request("capture.screenshot", body).is_err(), "{body}");
        }
        assert!(
            request(
                "capture.start",
                r#"{"region":{"x":0,"y":0,"width":1,"height":1}}"#
            )
            .is_err()
        );
    }
    #[test]
    fn status_surfaces_the_blob_reference_additively() {
        let plain = Status::default().value();
        assert!(plain["blob"].is_null());
        assert!(plain["blob_error"].is_null());
        let id = format!("b3:{}", "0".repeat(64));
        let reference = blob::parse_reference(
            &json!({"blob":id,"size":9,"mime":"image/png","name":"a.png","origin":"alpha"})
                .to_string(),
        )
        .unwrap();
        let uploaded = Status {
            blob: Some(reference),
            ..Default::default()
        }
        .value();
        assert_eq!(uploaded["blob"]["blob"], format!("b3:{}", "0".repeat(64)));
        assert_eq!(uploaded["blob"]["mime"], "image/png");
        assert!(uploaded["blob_error"].is_null());
        let failed = Status {
            blob_error: Some("lane answered 413 for http://10.42.0.5:4210/blob".into()),
            ..Default::default()
        }
        .value();
        assert!(failed["blob"].is_null());
        assert_eq!(
            failed["blob_error"],
            "lane answered 413 for http://10.42.0.5:4210/blob"
        );
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

    fn parked_job(park: impl FnOnce() + Send + 'static) -> Job {
        let cancel = Arc::new(AtomicBool::new(false));
        let settled = Arc::new(AtomicBool::new(false));
        let thread = std::thread::spawn(park);
        Job {
            cancel,
            settled,
            thread,
        }
    }

    fn reference() -> Value {
        json!({"blob": format!("b3:{}", "0".repeat(64)), "size": 9, "mime": "image/png"})
    }

    #[test]
    fn settle_lands_the_reference_on_a_published_capture() {
        let status = Arc::new(Mutex::new(Status {
            generation: 1,
            ..Default::default()
        }));
        let settled = Arc::new(AtomicBool::new(false));
        let expected = reference();
        settle_job(
            &status,
            1,
            &settled,
            Some(move || Ok(expected)),
            Ok(()),
        );
        let state = status.lock().unwrap();
        assert_eq!(state.phase, "complete");
        assert!(state.error.is_none());
        assert_eq!(state.blob, Some(reference()));
        assert!(settled.load(Ordering::Relaxed));
    }

    #[test]
    fn a_published_duration_failure_still_uploads() {
        // The MP4 landed at `path`, so the store copy is owed even
        // though encoding then reported the shortfall.
        let status = Arc::new(Mutex::new(Status {
            generation: 1,
            ..Default::default()
        }));
        let settled = Arc::new(AtomicBool::new(false));
        let expected = reference();
        settle_job(
            &status,
            1,
            &settled,
            Some(move || Ok(expected)),
            Err("encoder fell behind: 299 frames".into()),
        );
        let state = status.lock().unwrap();
        assert_eq!(state.phase, "failed");
        assert_eq!(state.error.as_deref(), Some("encoder fell behind: 299 frames"));
        assert_eq!(state.blob, Some(reference()));
    }

    #[test]
    fn a_stale_upload_never_lands_in_a_newer_jobs_status() {
        let status = Arc::new(Mutex::new(Status {
            generation: 2,
            ..Default::default()
        }));
        let settled = Arc::new(AtomicBool::new(false));
        let expected = reference();
        settle_job(
            &status,
            1,
            &settled,
            Some(move || Ok(expected)),
            Ok(()),
        );
        let state = status.lock().unwrap();
        assert!(state.blob.is_none());
        assert!(state.blob_error.is_none());
    }

    #[test]
    fn blob_pending_marks_an_in_flight_dual_write() {
        // Nothing published, nothing owed.
        assert!(!Status::default().value()["blob_pending"]
            .as_bool()
            .unwrap());
        // A capture that failed without publishing owes no upload.
        let status = Arc::new(Mutex::new(Status {
            generation: 1,
            ..Default::default()
        }));
        let settled = Arc::new(AtomicBool::new(false));
        settle_job(
            &status,
            1,
            &settled,
            None::<fn() -> Result<Value, String>>,
            Err("compositor gone".into()),
        );
        assert!(!status.lock().unwrap().value()["blob_pending"]
            .as_bool()
            .unwrap());
        // Published: pending from settle until the upload lands a field.
        let status = Arc::new(Mutex::new(Status {
            generation: 1,
            ..Default::default()
        }));
        let settled = Arc::new(AtomicBool::new(false));
        let in_flight = status.clone();
        settle_job(
            &status,
            1,
            &settled,
            Some(move || {
                assert!(in_flight.lock().unwrap().value()["blob_pending"]
                    .as_bool()
                    .unwrap());
                Err("lane answered 413 for http://10.42.0.5:4210/blob".into())
            }),
            Ok(()),
        );
        let landed = status.lock().unwrap().value();
        assert!(!landed["blob_pending"].as_bool().unwrap());
        assert!(landed["blob_error"].is_string());
    }

    #[test]
    fn admission_frees_at_the_terminal_phase_while_the_upload_runs() {
        let status = Arc::new(Mutex::new(Status {
            generation: 1,
            ..Default::default()
        }));
        let settled = Arc::new(AtomicBool::new(false));
        let (release, parked) = std::sync::mpsc::channel::<()>();
        let flag = settled.clone();
        let state = status.clone();
        let thread = std::thread::spawn(move || {
            settle_job(&state, 1, &flag, Some(|| { let _ = parked.recv(); Ok(reference()) }), Ok(()));
        });
        let mut slot = Some(Job {
            cancel: Arc::new(AtomicBool::new(false)),
            settled,
            thread,
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while !slot_free(&slot) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(slot_free(&slot), "slot frees at the terminal phase");
        assert!(
            !slot.as_ref().unwrap().thread.is_finished(),
            "the dual-write is still in flight"
        );
        // The next capture takes the slot and resets the status under
        // its own generation; the parked uploader then finishes stale.
        let taken = slot.take().unwrap();
        status.lock().unwrap().generation = 2;
        drop(release);
        let Job { thread, .. } = taken;
        thread.join().unwrap();
        let state = status.lock().unwrap();
        assert!(state.blob.is_none(), "stale result dropped, not landed");
    }

    #[tokio::test]
    async fn shutdown_reaps_a_worker_that_honours_the_flag() {
        // A worker mid-upload ignores `cancel` (a stopped recording
        // still uploads) and exits on `shutdown` — the reap case.
        let shutdown = Arc::new(AtomicBool::new(false));
        let flag = shutdown.clone();
        let job = parked_job(move || {
            while !flag.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        assert!(
            reap(job, &shutdown, Duration::from_secs(5)).await,
            "worker honours shutdown, must join within the bound"
        );
    }

    #[tokio::test]
    async fn shutdown_gives_up_on_a_stuck_worker_within_the_bound() {
        // A worker parked past the bound is abandoned, not waited out;
        // it exits shortly after so the blocking pool drains too.
        let shutdown = Arc::new(AtomicBool::new(false));
        let job = parked_job(|| std::thread::sleep(Duration::from_secs(2)));
        let started = Instant::now();
        assert!(!reap(job, &shutdown, Duration::from_millis(150)).await);
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
