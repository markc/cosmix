# cosmix-bg-showcase

An exploratory native Wayland client that renders procedural and Blender-authored Bevy 3D
scenes on input-free Background layer surfaces. It is a separate process from
the compositor and reuses `cosmix-shell-host` for output and render lifetimes.

The host keeps an absolute frame deadline across ordinary scheduling delays,
skips missed slots and retains compositor callback backpressure. Boing renders
interpolated poses between its 120 Hz physics steps; collisions and kicks
continue to use the authoritative physics state. `wallpaper.status` exposes
client submission interval totals, maximum and buckets (at most 20, 40, 60,
or over 60 milliseconds) for measuring pacing. These are client submission
measurements, not physical display presentation timestamps.

| Scene | Demonstrates |
| --- | --- |
| `boing` | Red/white checked spinning sphere, grid stage and shadows, with Avian gravity and elastic collisions |
| `bloom` | Bevy's emissive sphere field with bloom and coordinated vertical motion |
| `shapes` | Bevy's rotating primitives and extrusions with generated UV palette and shadows |
| `boids` | Paper-dart flocking with native pointer repulsion, window avoidance and persistent preferences |
| `primitives` | Coloured metallic forms, rotating meshes, shadows and camera motion |
| `orbits` | Shared meshes, emissive materials, bloom and independent orbital motion |
| `mist` | Procedural pillars, distance fog and an orbiting camera |
| `observatory` | Blender-authored GLB geometry and PBR materials, with metadata-driven ECS motion |
| `celestial` | Floating celestial machine with nested satellite turbines, toothed annuli and 50 ECS motion pivots |
| `coast` | CC0 coastal sky and sand maps, fixed viewpoint, foreground rocks and gentle reflective waves |

`bloom` and `shapes` are minimal adaptations of Bevy 0.19.0's
[`bloom_3d`](https://github.com/bevyengine/bevy/blob/v0.19.0/examples/3d/bloom_3d.rs)
and [`3d_shapes`](https://github.com/bevyengine/bevy/blob/v0.19.0/examples/3d/3d_shapes.rs).
They retain the upstream geometry, materials, lighting and fixed camera poses;
keyboard controls, labels and wireframe toggles are omitted. Their animation
uses admitted per-output time. Both generate everything at runtime and require
no external media. The adapted source includes Bevy's MIT licence notice.

The earlier scenes' techniques are drawn from Bevy's `3d_shapes`, `bloom_3d` and
`atmospheric_fog` examples. The arrangements and sculptures are original.
Coast uses CC0 Poly Haven source material, with provenance in the package's
`media-manifest.json` and `MEDIA-LICENSE.md`.

Build the selected package from the desktop workspace with
`cargo build -p cosmix-bg-showcase`. Bevy defaults are disabled; the selected
client uses Wayland and no X11 backend. Do not build it as part of an unrelated
X11-enabled workspace selection when verifying that feature boundary.

Run with an explicit native `WAYLAND_DISPLAY`:

```text
cosmix-bg-showcase --scene orbits --fps 30 --seconds 30
cosmix-bg-showcase --scene bloom --fps 30 --seconds 30
cosmix-bg-showcase --scene shapes --fps 30 --seconds 30
cosmix-bg-showcase --scene bloom --camera orbit --fps 30 --seconds 120
cosmix-bg-showcase --scene boing --camera fixed --fps 30 --seconds 120
cosmix-bg-showcase --scene boing --camera fixed --fps 30 --seconds 0
cosmix-bg-showcase --scene mist --seconds 60 --capture /tmp/mist-preview.png
cosmix-bg-showcase --scene coast --media-root /path/to/media --seconds 60
```

`bloom`, `shapes` and `boing` accept `--camera fixed|orbit`. Fixed is the default and
preserves the upstream view. Orbit starts at that same pose and makes one
horizontal revolution in 120 admitted seconds for bloom/shapes, maintaining height, distance
and the look-at target. It has no zoom or roll. Camera motion pauses with the
output's admitted scene time. Other scenes reject an explicit `--camera`
option and retain their existing camera behaviour.

## Boing physics study

Boing generates its own broad red/white checker texture, tilted spinning sphere,
grey stage and purple grid. This original project imagery is dedicated to
[CC0](https://creativecommons.org/publicdomain/zero/1.0/); Rust code retains MIT.
No external binary assets or downloaded historical demo files are used.

[Avian 0.7](https://github.com/Jondolf/avian/tree/v0.7.0) supplies the sphere's
rigid-body dynamics: gravity, frictionless floor and side colliders, restitution
and angular velocity. Translation is constrained to the stage's vertical plane.
There is no scripted bounce trajectory or manual collision reversal. Each output
owns a separate headless physics App, so outputs cannot collide or advance one
another. Fixed 1/120-second steps with four solver substeps consume only admitted
output time; missed time is bounded and never replayed after suspension.

Boing's optional orbit uses a front-facing +/-20-degree arc over 120 seconds to
keep the grid wall behind the subject. The fixed view is the default. A shadowed
point light illuminates the ball and stage; bloom is not used. Output removal
drops its simulation and geometry. Logs report initialisation and floor/side
impact counts when a simulation is dropped. This remains a bounded visual demo,
not a claim of complete desktop idle/coverage policy or exact energy conservation.

Boing also registers the native Bus service `bg-showcase`. Send `boing.kick`
with an empty object to request a bounded physical impulse on each live output;
`boing.status` reports the output count and accepted kick count. For example,
from Mix:

```text
send bg-showcase boing.kick timeout=5
send bg-showcase boing.status timeout=5
```

The control worker wakes the native scene loop through `SceneWake`; it does not
poll or give the background keyboard focus. Requests are bounded and kicks are
limited to one every 250 ms. Acceptance means the impulse is queued; physics
advances it only when the output receives admitted simulation time. A missing
Bus connection leaves the visual preview running but makes remote kicks
unavailable. Run only one Boing preview per local service namespace.

For an opt-in compositor shortcut, start the preview's Cosmix compositor with
`--f9-bus bg-showcase boing.kick`. F9 then sends the request through noded/ABP;
the background itself remains input-free. This is a generic compositor Bus
binding, not simulated keyboard input into the scene.

`--seconds` accepts 1–300 for a bounded preview (default 30), or explicit `0`
to run continuously until stopped. Continuous mode has no expiry deadline;
output admission still controls rendering and animation, and SIGTERM follows
the host's normal render-handle teardown. `--fps` accepts 1–60, subject to
compositor callbacks. Capture requires a nonzero duration and saves only this
client's first output after 60 admitted frames, then exits. The PNG path must
be absolute and must not exist. An unsuccessful capture or expired capture
deadline is an error.

The four catalogue demos are `boing`, `bloom`, `shapes` and `boids`.
One running client selects among them through the native `bg-showcase` Bus service:

| Verb | Body | Result |
| --- | --- | --- |
| `background.list` | `{}` | `scenes` entries with `id`, `title` and boolean orbit-camera support |
| `background.status` | `{}` | Selected `scene`, `camera`, configured `msaa` samples and output count |
| `background.select` | `{"scene":"bloom","camera":"orbit","msaa":4}` | Accepted selection; camera defaults to `fixed`; omitted MSAA retains current quality |

`--msaa 1|4` selects no multisampling or four samples (the existing default).
Lowering it can reduce GPU bandwidth at high resolutions, at the cost of less
smooth edges. To compare without restarting, select the current scene and
camera with `"msaa":1`, then `"msaa":4`. A change to MSAA alone preserves scene
entities, physics and animation time; allow rendering pipelines to warm up
before comparing frame intervals. The setting applies to every output and
survives scene switches. Two and eight samples are excluded until the host
validates their support for the active colour and depth formats.

For presentation pacing comparisons, SceneHost also accepts
`COSMIX_SCENE_PRESENT_MODE=fifo|auto-no-vsync`. FIFO remains the default.
AutoNoVsync prefers Immediate, then Mailbox, then FIFO according to adapter
support. The host still permits only one outstanding native frame callback;
the setting changes the client's presentation mode, not compositor scanout.
Compare nested and direct VT sessions separately; a result on one does not
establish performance on the other.

Selection keeps the host's output surfaces, camera entities and render targets.
It retires the previous scene's entities, output-local simulations and clocks,
clears camera effects and projection, then creates one replacement scene per
output. Boids has a fixed orthographic view. A capture preview rejects selection
so its output cannot silently change subject. Acceptance reports selection
of ECS content, not a guarantee that a frame has already reached the display.

The four demos share the host in `apps/bg-showcase/src`. Boing geometry and
Avian simulation live in `crates/cosmix-bg-boing`, also used by the compositor's
optional native HUD comparison. This crate has no window-system or Bus dependency.
The library's
`boids` module contains the former wallpaper rendering, preferences, geometry,
pointer and coverage logic; `cosmix-wallpaper` is a compatibility entry point
into that same implementation. Flocking mathematics remain in the pure
`cosmix-flock` crate. Flat unlit darts use the same 3D camera pipeline as the
other demos, with an orthographic projection.

### Experimental compositor-native comparison

Build `cosmix-comp` with `kms-live,hud-probe` and launch its KMS client-content
mode with `COSMIX_COMP_HUD_PROBE=1`. Boing and a translucent Bevy UI panel share
the compositor renderer. The panel automatically opens, holds and closes on a
six-second cycle, using the configured output scale. Do not start the separate
showcase background in this comparison: it would cover the native scene.

This opt-in test targets the first output. It retains normal Wayland application
composition and final-output capture. Locking disables the 3D camera and HUD;
output deactivation and retirement also disable or remove them. Quoin is not
rewritten by this experiment. Existing `bg-showcase` controls, including F9 kick,
control the external showcase only. `COSMIX_FRAME_TRACE=1` records compositor
timings; `FRAME_TEST native_hud_phase` identifies the automatic panel phases.

Boids retains `wallpaper.status` and `wallpaper.props.*` on the
`bg-showcase` service, including persisted pause, palette, density, speed and
pointer/window settings. Its watch reply advertises
`bg-showcase.props.changed`; the compatibility executable uses the original
`wallpaper` service and topic. Boids requires authoritative `comp` geometry
before animating, and keeps the clear-before-suspend coverage handshake.
The other three demos retain native output callback pacing; full geometric
coverage suspension remains specific to boids.

## Scene interface

`configure_scene_host_with_config` accepts a `SceneHostConfig` containing frame
rate, surface namespace, window title and `SceneCameraKind`. Enable the
`scene-3d` feature to select `ThreeD`; the existing `configure_scene_host` call
continues to create 2D wallpaper cameras.

The host publishes output identity and dimensions through `SceneViews` and
admitted output names through `SceneTick`. A scene advances only admitted
outputs. It owns its content entities and camera pose; the host owns the
camera entity, target, render layer, activation and teardown. Lights and meshes
must use the output's render layer, and output replacement must retire its old
content. `SceneFrameResults` reports real submissions after rendering.

The shared background service translates native ABP selection into ECS
resources. Pixel presentation remains Wayland; Bus is the semantic control
surface, not a stream of per-frame draw commands.

## Blender authoring proof

`--scene observatory` loads `kinetic-observatory.glb` from the external media
root. The editable source is `source/kinetic-observatory.blend` in that library;
the package's `assets-src/observatory.py` recreates it inside Blender. From the package directory, run
`/opt/cosmix/bin/mix assets-src/export.mix` to regenerate both assets. Blender
is an authoring dependency only; the installed preview needs the external
media library and does not depend on its working directory. Python is used only inside Blender for
this asset pipeline, with Mix handling orchestration.

The scene contains three metallic orbital assemblies with emissive traces,
markers, a central core and a plinth. Each orbital pivot carries glTF node
extras of the form:

```json
{"cosmix_bg":{"version":1,"axis":"y","radians_per_second":0.18}}
```

Axes refer to the exported glTF local coordinates. The importer accepts only
this narrow schema, validates the motion rate and preserves authored local
rotation. Rust ECS systems advance the motion on admitted output ticks.
Imported cameras, lights and animation players are disabled; the native host
retains output control. Every imported descendant receives the output render
layer. Capture warm-up starts after hierarchy and asset readiness.

This proves geometry, hierarchy, PBR materials, emissive materials and custom
properties. It does not yet test skeletal animation, textures, physics or
arbitrary Blender node networks. Blender source regeneration is repeatable;
byte-identical `.blend`/GLB exports are not promised.

## Celestial Engine

`--scene celestial` selects `celestial-engine.glb` from the media root:
a faceted reactor, two open gyroscope cages, two toothed chronometer rings,
eight satellite machines with counter-rotating turbines and moving vanes, and
two polar crowns. The composition floats against black with bloom and a slow
camera orbit. Nested spin components combine orbital and local rotation.

The GLB has 313 nodes, 263 mesh objects, six materials and 50 motion pivots,
and occupies approximately 1.55 MB. It uses the same validated metadata and
native per-output lifecycle as the observatory. This is a visual complexity
demo; the larger mesh count is not an idle-power or performance acceptance claim.

Editable source: `source/celestial-engine.blend` in the media root. Regenerate from the
package directory with `/opt/cosmix/bin/mix assets-src/export-celestial.mix`.
The generator overwrites that source file and `celestial-engine.glb` in the media root.
Python executes only inside Blender; no Python is required by the preview.

## External media and coastal study

Binary media is no longer embedded in the executable or committed with this
package. `--media-root` accepts an absolute directory; the default is
`$XDG_DATA_HOME/cosmix/media`, falling back to `$HOME/.local/share/cosmix/media`.
Original procedural scenes still require no media. The preview performs no
network downloads. Missing required assets fail explicitly.

Coast needs `coast/skybox.png`, `coast/sand-basecolor.png` and
`coast/sand-normal.png` under that root. `assets-src/coast-sky.py` converts the
manifest's Cape Hill EXR into a six-face tonemapped cubemap; `coast-materials.py`
prepares the two Coast Sand 03 texture maps. Execute these scripts only inside
Blender, invoked through Mix. Authoring scripts accept `COSMIX_MEDIA_ROOT` and
otherwise use the same XDG/HOME default. Downloaded originals live in `source/`.

The sky is photographed and static. Water geometry/normals change on admitted
output ticks; reflections use the tonemapped cubemap as an approximation,
not calibrated or prefiltered HDR illumination. This is an initial composition
study, not a photorealism or low-power acceptance claim.

See the [media-library proposal](../spec/media-library.md) for separating
catalogue metadata in Git from binary objects on native webd storage.
