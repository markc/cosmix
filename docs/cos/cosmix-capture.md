# Cosmix Capture

`cosmix-capture` is a native Wayland client and Cosmix Bus citizen. Quoin uses
its `capture` service to save complete output screenshots and full-screen MP4
videos. It requests the compositor's final composed output, including panels,
application windows and the cursor. The compositor remains the authority for
session locking; capture may show a lock-only frame, never bypass the lock.

Start with `cosmix-capture --directory /absolute/media/path --output OUTPUT`.
The directory defaults to `~/Videos/Cosmix`. Without an output name, the first
named advertised output is selected. This version records one entire output,
not a combined multi-monitor canvas. Keep the media directory outside Git.

The service exposes four commands, with JSON object bodies:

| Command | Body | Effect |
| --- | --- | --- |
| `capture.screenshot` | `{}` | Start an asynchronous PNG screenshot |
| `capture.start` | `{"fps":30}` | Start MP4 recording, 1–60 fps (default 30) |
| `capture.stop` | `{}` | Stop the active job and finalise its MP4 |
| `capture.status` | `{}` | Read state and completed file path |

Replies contain `recording`, `phase`, `path`, `error`, `frames`, `pid`,
`version`, `git_sha` and `build_time`. Phases are `idle`, `screenshot`,
`starting`, `recording`, `finalising`, `complete` and `failed`.
Only `complete` guarantees successful publication. A failed recording may
still have a finalised usable MP4; `error` explains why recording stopped.
The initial screenshot/start response acknowledges the job; poll status for
completion. Stop is idempotent when no job is active. Concurrent jobs fail
explicitly. Names are generated, and existing files are never overwritten.

Recording statistics also expose `fresh_frames`, `duplicate_frames`,
`elapsed_ms`, `fresh_fps`, `capture_ms` and `encode_ms`. Use `fresh_fps` to
measure acquisition throughput: the MP4 frame rate alone includes repeated
frames. Capture and encode times are cumulative milliseconds.

Recording automatically stops after five minutes. Capture requests have a
two-second deadline and output removal/disconnection stops capture. The
encoder receives raw pixels from this native client; FFmpeg is a codec child,
not the capture application. No X11 capture backend exists. FFmpeg must be
installed with the `libx264` encoder for the default software mode. It uses
H.264, two encoder threads and 4:2:0 output; odd pixel dimensions receive at
most one padding row/column. GPU-rendered scenes remain GPU-rendered.

Hardware H.264 encoding is an explicit option:
`cosmix-capture --vaapi-device /dev/dri/renderD128`. It requires access to that
render device and FFmpeg's `h264_vaapi` encoder. The native Wayland capture
path remains the same; FFmpeg converts to NV12, uploads to the selected device
and encodes there. Software `libx264` remains the default. A requested hardware
encoder failure fails the recording, with no silent software fallback.
`encoder`, `encoder_backend` and `vaapi_device` in status identify the selected
video encoding configuration, including before recording starts. Screenshots
still use the native PNG encoder regardless of the selected video codec.

A compositor may refuse an individual video frame during resource contention.
Video discards that request, increments `refused_frames` and retries at the
normal cadence; a screenshot refusal fails immediately. Video stops if no
successful response arrives for two seconds. Every valid `ready`, including
a repeated presentation timestamp on an idle desktop, resets that deadline;
refusals do not. Invalid layouts, output removal, disconnection and
protocol errors remain fatal.

The video producer overlaps up to three native screencopy requests on one
connection, paced at the requested frame rate. The depth respects the
compositor's 512 MiB per-client reservation budget, including its source,
staging and conversion buffers. It leaves room for one retiring request:
three active requests at 4K and smaller sizes, one at 5K. The compositor
also caps reservations at 1 GiB globally and requests at four per client,
eight globally; the client does not bypass these limits.
An ordered FIFO of at most three frames separates capture from encoding.
Before the movie clock starts, it waits for two following frames while keeping
the initial image as the first video frame. This small preroll absorbs arrival
jitter without discarding adjacent fresh images; `starting` includes the
preroll, and recording elapsed time excludes it. Sustained overload drops the
oldest queued frame and counts it, rather than accumulating latency. Screenshots
remain single requests. Output dimension changes stop the recording before
allocating a new set of buffers.

Successful video requests reuse their capture-owned SHM backing files, pools
and buffers only after `ready` and the complete CPU read. This screencopy lane
does not use `wl_buffer.release`; `ready` marks the completed copy. Occupied
and free slots together are bounded by the pipeline depth, with exact format,
dimensions and stride matching. Failed requests discard their slots. Each
backing file has a persistent read-only mapping and is sealed against resizing.
Colour conversion reads that mapping only after `ready` and finishes before
the slot can be reused, avoiding an intermediate full-frame CPU copy. Queued
RGBA frames remain independently owned. Screenshots keep a single-use allocation.

Status distinguishes successful pixel acquisitions (`acquired_frames`),
unique presentations actually submitted to the encoder (`fresh_frames`, `fresh_fps`),
overwritten images (`dropped_frames`), repeated presentation timestamps and
encoded slots without a new presentation (`duplicate_frames`). Every successful
response supplies its actual pixels, because the cursor overlay can change
even with the same presentation timestamp. Repeated responses satisfy preroll
but never inflate the unique presentation count. `frames` includes all slots. Timing
fields split capture waiting, SHM reads, normalisation and encoder writes.
With mapped SHM, `capture_read_ms` is zero and mapped reads are included in
`capture_normalise_ms`;
overlapping request waits are cumulative latency, not exclusive wall time.
Recording elapsed time stops before encoder finalisation.

The pipeline applies encoder backpressure deadlines and closes/reaps its
encoder on shutdown.
Missed recording slots duplicate the most recent successful capture to
preserve playback duration; no further frames are generated after capture
fails; a write already in progress may finish. If encoding falls behind and
the resulting file is materially shorter than wall time, the saved MP4 is
reported as `failed` with an explicit duration error. PNGs preserve full output
pixel dimensions and opaque RGBA colours.
Partial files remain clearly named `.partial.png`/`.partial.mp4` after failure
and are never presented as successful completed captures.

Captured content retains its existing rights. Original project demo media can
be dedicated to CC0; screenshots containing unrelated applications or media
must not automatically be labelled CC0.
