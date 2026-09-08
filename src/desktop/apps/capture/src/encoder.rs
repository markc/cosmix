use crate::wayland::Pixels;
use std::{
    ffi::OsString,
    fs::OpenOptions,
    io::Write,
    os::fd::AsRawFd,
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

/// A single bounded raw frame travels directly into the encoder's pipe.
/// Nonblocking writes prevent a wedged codec from trapping shutdown.
pub struct Encoder {
    child: Child,
    input: Option<ChildStdin>,
    dimensions: (u32, u32),
    finished: bool,
}
impl Encoder {
    pub fn new(
        path: &Path,
        pixels: &Pixels,
        fps: u32,
        vaapi_device: Option<&Path>,
    ) -> Result<Self, String> {
        // Atomically own the inode before starting the codec. The child opens
        // only its inherited descriptor, never an attacker-replaceable path.
        // A regular file retains seeking required for MP4 faststart.
        let output = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|e| format!("reserve MP4 partial without overwrite: {e}"))?;
        let mut command = Command::new("ffmpeg");
        if let Some(device) = vaapi_device {
            command.arg("-vaapi_device").arg(device);
        }
        let mut child = command
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-filter_threads",
                "2",
                "-y",
                "-f",
                "rawvideo",
                "-pixel_format",
                "rgba",
                "-video_size",
                &format!("{}x{}", pixels.width, pixels.height),
                "-framerate",
                &fps.to_string(),
                "-i",
                "pipe:0",
                "-an",
            ])
            .args(codec_args(vaapi_device.is_some()))
            .args(["-movflags", "+faststart", "-f", "mp4"])
            .arg("/proc/self/fd/1")
            .stdin(Stdio::piped())
            .stdout(Stdio::from(output))
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("start ffmpeg encoder: {e}"))?;
        let input = child.stdin.take().ok_or("encoder stdin missing")?;
        // A 4K RGBA frame is 32 MiB. A small pipe forces thousands of
        // producer/decoder wakeups per frame. Request a bounded 1 MiB pipe;
        // restricted systems retain their default without losing correctness.
        // SAFETY: input owns this live pipe descriptor; no pointer arguments.
        let _ = unsafe { libc::fcntl(input.as_raw_fd(), libc::F_SETPIPE_SZ, 1024 * 1024) };
        // SAFETY: live owned pipe descriptor, preserve existing flags.
        let flags = unsafe { libc::fcntl(input.as_raw_fd(), libc::F_GETFL) };
        if flags < 0
            || unsafe { libc::fcntl(input.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }
                < 0
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err("cannot make encoder pipe nonblocking".into());
        }
        Ok(Self {
            child,
            input: Some(input),
            dimensions: (pixels.width, pixels.height),
            finished: false,
        })
    }
    pub fn write(&mut self, pixels: &Pixels, cancel: &AtomicBool) -> Result<(), String> {
        if self.dimensions != (pixels.width, pixels.height) {
            return Err("output dimensions changed during recording".into());
        }
        let input = self.input.as_mut().ok_or("encoder closed")?;
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut bytes = pixels.rgba.as_slice();
        while !bytes.is_empty() {
            // Finish an already-started frame on user cancellation, so final MP4
            // has no truncated raw frame. The hard deadline still bounds this.
            if Instant::now() >= deadline {
                return Err("encoder backpressure deadline elapsed".into());
            }
            if bytes.len() == pixels.rgba.len() && cancel.load(Ordering::Relaxed) {
                return Err("capture cancelled".into());
            }
            match input.write(bytes) {
                Ok(0) => return Err("encoder pipe closed".into()),
                Ok(n) => bytes = &bytes[n..],
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    let mut fd = libc::pollfd {
                        fd: input.as_raw_fd(),
                        events: libc::POLLOUT,
                        revents: 0,
                    };
                    // SAFETY: input owns the descriptor, valid one-element array.
                    unsafe {
                        libc::poll(&mut fd, 1, 50);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(format!("encoder write: {e}")),
            }
        }
        Ok(())
    }
    pub fn finish(&mut self) -> Result<(), String> {
        self.input.take();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.child.try_wait().map_err(|e| e.to_string())? {
                Some(status) => {
                    self.finished = true;
                    return if status.success() {
                        Ok(())
                    } else {
                        Err(format!("encoder exited {status}"))
                    };
                }
                None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
                None => return Err("encoder finalisation deadline elapsed".into()),
            }
        }
    }
}

fn codec_args(vaapi: bool) -> Vec<OsString> {
    let args: &[&str] = if vaapi {
        &[
            "-vf",
            "pad=ceil(iw/2)*2:ceil(ih/2)*2,format=nv12,hwupload",
            "-c:v",
            "h264_vaapi",
            "-qp",
            "23",
        ]
    } else {
        &[
            "-vf",
            "pad=ceil(iw/2)*2:ceil(ih/2)*2",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-threads",
            "2",
            "-pix_fmt",
            "yuv420p",
        ]
    };
    args.iter().map(OsString::from).collect()
}
impl Drop for Encoder {
    fn drop(&mut self) {
        if !self.finished {
            self.input.take();
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_partial_is_rejected_before_spawning_encoder() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("existing.partial.mp4");
        std::fs::write(&path, b"keep existing data").unwrap();
        let pixels = Pixels {
            width: 1,
            height: 1,
            rgba: vec![0, 0, 0, 255],
        };
        assert!(Encoder::new(&path, &pixels, 30, None).is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"keep existing data");
    }
    #[test]
    fn explicit_vaapi_selects_hardware_codec_without_software_fallback() {
        let hardware = codec_args(true);
        assert!(hardware.contains(&OsString::from("h264_vaapi")));
        assert!(!hardware.contains(&OsString::from("libx264")));
        assert!(
            hardware
                .iter()
                .any(|arg| arg.to_string_lossy().contains("format=nv12,hwupload"))
        );
        let software = codec_args(false);
        assert!(software.contains(&OsString::from("libx264")));
        assert!(!software.contains(&OsString::from("h264_vaapi")));
    }
}
