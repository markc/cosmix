# CosMix Media

`cosmix-media` is a Wayland-native Bevy/CTK player for local audio and video.
Version 0.1.1 uses GStreamer inside the process for demuxing, decoding and a
shared audio/video playback clock. Audio uses `pulsesink`, compatible with
PipeWire's PulseAudio server. It does not launch mpv, FFmpeg or an X11 window.

Run `cosmix-media /absolute/path/movie.mp4`, or start without a file and choose
one with **File → Open…** (Ctrl+O). The native CTK file chooser opens initially
in `--directory DIR` (default `$HOME/Downloads`) and remembers the last selected
directory for this session.
`--service NAME` changes the default Bus name `media` for multiple instances.

The conventional menu bar replaces DCS panel furniture: **File** contains Open
and Quit; **Playback** contains play/pause, stop and ten-second relative seeks;
**Audio** contains volume and mute; **View** contains fullscreen. The video fills
the remaining area above a small status row. There are no sidebars, pin/float
controls or separate button toolbar.

Space toggles playback; Left/Right seek; M toggles mute; F toggles fullscreen.
View → Toggle fullscreen performs the same action; Escape exits fullscreen.
Fullscreen hides the player's menu and status bar, leaving the video fitted
inside the output with its aspect ratio preserved.
Playback shortcuts are suspended while the file chooser is active. Files can
also be opened through the Bus. Playlists, subtitle controls and network URLs
are not implemented.

## Runtime requirements

The Wayland session must be available through `WAYLAND_DISPLAY`. CTK builds
with its Wayland platform feature; no X11 platform feature is requested.
Audio follows `PULSE_SERVER` when set, otherwise the normal PulseAudio client
discovery path. Opening a container audio socket requires the session host to
provide that socket and access permissions.

GStreamer 1.x runtime plugins must provide `playbin`, `appsink`, `pulsesink`,
MP4 demuxing and the codecs in the files. On Arch, the baseline packages are
`gstreamer`, `gst-plugins-base`, `gst-plugins-good` and `gst-libav`. The Rust
application is MIT licensed; the separately installed GStreamer libraries and
plugins retain their upstream licences. This is not a pure-Rust codec stack.

MP3 and H.264/AAC MP4 are the initial acceptance formats. An MP4 extension does
not guarantee codec support. Other installed GStreamer codecs may also work.
Errors are visible in the status bar and Bus status; unsupported files are not
reported as successful playback.

## Bus control

Commands use JSON object bodies. A mutation reply means the playback worker
applied the operation or initiated the asynchronous pipeline state change;
read `media.status` for subsequent codec errors and progress. A timeout does
not cancel an operation already queued. All controls use the same worker queue
as the UI. This initial service exposes local file paths to its Bus callers;
deploy it on the trusted desktop Bus.

| Command | Body |
|---|---|
| `media.open` | `{"path":"/absolute/path/movie.mp4"}` |
| `media.play`, `media.pause`, `media.toggle`, `media.stop` | `{}` |
| `media.seek` | `{"seconds":30}` (absolute, keyframe seek) |
| `media.volume` | `{"value":0.8}` (0–1) |
| `media.mute`, `media.fullscreen` | `{"value":true}` |
| `media.fullscreen.toggle` | `{}` |
| `media.status` | `{}` |
| `media.props.get` | `{"path":"position"}` or `{}` for the complete snapshot |
| `media.quit` | `{}` |

Status contains phase, path, position/duration in seconds, volume, muted,
fullscreen, video dimensions, generation, frame counters, backend, version,
PID and error. `playing` describes the requested pipeline state; advancing
position and video frames are the evidence of actual playback. `ended` is EOS.
`decoded_frames` counts frames delivered to the application, including preroll;
`replaced_frames` counts those superseded before Bevy consumed them. These are
not hardware decoder throughput or compositor presentation statistics.
`fullscreen` is also a requested state: the compositor must honour the Wayland
fullscreen request. CosMix Comp 0.51.1 applies fullscreen at the acknowledged
surface commit, hides compositor decorations and focused native Quoin panels,
and restores the previous window geometry on exit. The compositor's
`windows.<id>.fullscreen` property reports the committed state; the player's
flag reports the request. Earlier compositor versions only acknowledged it.

For example, from Mix on a desktop Bus:

```mix
send media media.fullscreen.toggle
send media media.toggle
send media media.fullscreen value=false
```

The menu, keyboard and Bus all enqueue the same worker actions. Two queued
fullscreen toggles therefore cancel each other even before the UI refreshes.

## Rendering and limits

Decoding, pipeline state changes and seeks run on a dedicated worker. The
command queue holds at most 32 requests. Appsink retains at most two frames;
the renderer handoff retains only the latest frame. A stalled window renderer
therefore drops obsolete video instead of blocking audio or growing a queue.
Decoded output is bounded to 4096 × 4096; this does not bound every allocation
inside third-party demuxers/decoders.

This baseline copies RGBA pixels into Bevy images. It is **not zero-copy** and
does not guarantee hardware decoding. Future VA-API/DMA-BUF or Vulkan Video
backends must preserve audio/video clocking, seek/EOS handling and bounded
frame ownership while replacing this handoff. Matching wgpu versions alone
does not establish compatible device creation or decoder support.
