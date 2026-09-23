//! Comp-owned corner-hotspot affordance (Quoin shell design §2, §8.1, §8.5,
//! §8.7): the hover reveal of an engaged hotspot, the flash acknowledging a
//! recognised release, and the slow first-run discovery flash.
//!
//! The protocol thread publishes a coalesced [`View`] in global LOGICAL
//! units. This system draws world-space sprites, which the output cameras'
//! logical projections rasterise at each output's own scale, so a hotspot is
//! the same logical size on a 2x output as beside it on a 1x output (Bevy UI
//! would not be: it reads scale 1.0 for every KMS texture target).
//!
//! Idle neutrality: the frame is recomputed every update, but the scene
//! revision advances only when the drawn frame differs from the last one.
//! A settled hover is one frame in and one out; the release flash is a
//! bounded one-shot of [`FLASH_STEPS`] quantised levels; the discovery flash
//! is a two-state blink, two frames per period, and ends at the first reveal.
use crate::compositor_scene::{
    CompositorSceneSet, LockBlankScene, LogicalCanvasSize, RendererOutputScale120,
    SceneContentRevision, renderer_rect,
};
use bevy::prelude::*;
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// Above every client band (content tops out at 900, the lock blank's
/// fallback is 925) and below the software cursor (950): no client and no
/// panel can stack above a hotspot (§2).
const HOTSPOT_Z: f32 = 940.0;
pub(crate) const FLASH_DURATION: Duration = Duration::from_millis(180);
pub(crate) const FLASH_STEPS: u8 = 6;
pub(crate) const DISCOVERY_PERIOD: Duration = Duration::from_millis(2_000);
pub(crate) const DISCOVERY_ON: Duration = Duration::from_millis(700);
const HOVER_ALPHA: f32 = 0.45;
const FLASH_ALPHA: f32 = 0.9;
const DISCOVERY_ALPHA: f32 = 0.6;

/// One hotspot square in global logical coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Square {
    pub x: f64,
    pub y: f64,
    pub side: f64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct View {
    /// Affordance enabled and corners live; false draws nothing at all.
    pub enabled: bool,
    /// Every hotspot of every output, four per output in `Corner::ALL` order.
    pub squares: Vec<Square>,
    /// Index into `squares` of the engaged hotspot.
    pub hover: Option<usize>,
    /// The last recognised release: which square, and when.
    pub flash: Option<(usize, Instant)>,
    /// Phase origin of the discovery blink while it runs.
    pub discovery: Option<Instant>,
}

/// One drawn square: its geometry and a quantised alpha level (0..=255).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Quad {
    pub square: Square,
    pub level: u8,
}

impl View {
    /// What to draw at `now`. Pure and quantised, so equal inputs within one
    /// animation step compare equal and cost no render.
    pub(crate) fn frame(&self, now: Instant) -> Vec<Quad> {
        if !self.enabled || self.squares.is_empty() {
            return Vec::new();
        }
        let discovery_on = self.discovery.is_some_and(|since| {
            let phase = now.saturating_duration_since(since).as_millis()
                % DISCOVERY_PERIOD.as_millis();
            phase < DISCOVERY_ON.as_millis()
        });
        let flash = self.flash.and_then(|(index, at)| {
            let elapsed = now.saturating_duration_since(at);
            (elapsed < FLASH_DURATION).then(|| {
                // Step 0 is full brightness, the last step the dimmest.
                let step = (elapsed.as_micros() * u128::from(FLASH_STEPS)
                    / FLASH_DURATION.as_micros()) as u8;
                let remaining = f32::from(FLASH_STEPS - step) / f32::from(FLASH_STEPS);
                (index, FLASH_ALPHA * remaining)
            })
        });
        let mut quads = Vec::new();
        for (index, square) in self.squares.iter().enumerate() {
            let mut alpha: f32 = 0.0;
            if discovery_on {
                alpha = alpha.max(DISCOVERY_ALPHA);
            }
            if self.hover == Some(index) {
                alpha = alpha.max(HOVER_ALPHA);
            }
            if let Some((_, flash)) = flash.filter(|(flashed, _)| *flashed == index) {
                alpha = alpha.max(flash);
            }
            if alpha > 0.0 {
                quads.push(Quad {
                    square: *square,
                    level: (alpha * 255.0).round() as u8,
                });
            }
        }
        quads
    }
}

#[derive(Resource, Clone, Default)]
pub(crate) struct HotspotBridge(Arc<Mutex<View>>);

impl HotspotBridge {
    pub(crate) fn set(&self, view: View) {
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = view;
    }

    #[cfg(test)]
    pub(crate) fn view(&self) -> View {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn frame(&self, now: Instant) -> Vec<Quad> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).frame(now)
    }
}

pub(crate) fn install(app: &mut App) {
    app.init_resource::<HotspotBridge>()
        .add_systems(Startup, attach)
        .add_systems(First, draw.after(CompositorSceneSet));
}

fn attach(feed: Option<Res<crate::protocol::ClientSceneFeed>>, bridge: Res<HotspotBridge>) {
    if let Some(feed) = feed {
        feed.install_hotspot_bridge(bridge.clone());
    }
}

#[derive(Component)]
struct HotspotQuad;

#[allow(clippy::too_many_arguments)] // Bevy system parameters.
fn draw(
    mut commands: Commands,
    bridge: Res<HotspotBridge>,
    canvas: Res<LogicalCanvasSize>,
    scale: Option<Res<RendererOutputScale120>>,
    lock: Option<Res<LockBlankScene>>,
    theme: Option<Res<crate::decoration_scene::DecorationSceneTheme>>,
    quads: Query<Entity, With<HotspotQuad>>,
    mut drawn: Local<Vec<Quad>>,
    mut revision: ResMut<SceneContentRevision>,
    damage: Option<Res<crate::capture::OutputDamageJournal>>,
) {
    // The lock blank owns the screen; corners are reset under a lock anyway,
    // but a discovery blink must not draw over it either.
    let frame = if lock.is_some_and(|lock| lock.active()) {
        Vec::new()
    } else {
        bridge.frame(Instant::now())
    };
    if frame == *drawn {
        return;
    }
    for entity in &quads {
        commands.entity(entity).despawn();
    }
    let scale120 = scale.map_or(crate::backend::kms::OutputScale120::ONE.get(), |s| s.0);
    let accent = theme.map_or(Color::WHITE, |theme| theme.accent());
    let mut damaged = Vec::with_capacity(drawn.len() + frame.len());
    for quad in drawn.iter().chain(&frame) {
        let rect = renderer_rect(
            quad.square.x as f32,
            quad.square.y as f32,
            quad.square.side as f32,
            quad.square.side as f32,
            scale120,
        );
        damaged.push(crate::capture::DisplayedLogicalRegion {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
        });
    }
    for quad in &frame {
        let rect = renderer_rect(
            quad.square.x as f32,
            quad.square.y as f32,
            quad.square.side as f32,
            quad.square.side as f32,
            scale120,
        );
        commands.spawn((
            HotspotQuad,
            Sprite::from_color(
                accent.with_alpha(f32::from(quad.level) / 255.0),
                Vec2::new(rect.width, rect.height),
            ),
            Transform::from_xyz(
                rect.x + rect.width / 2.0 - canvas.0.x / 2.0,
                canvas.0.y / 2.0 - rect.y - rect.height / 2.0,
                HOTSPOT_Z,
            ),
        ));
    }
    revision.advance();
    if let Some(damage) = damage {
        damage.mark_base_logical_regions(&damaged);
    }
    *drawn = frame;
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIDE: f64 = 10.0;

    fn squares() -> Vec<Square> {
        // One 320x240 output: TL, TR, BL, BR.
        vec![
            Square { x: 0.0, y: 0.0, side: SIDE },
            Square { x: 310.0, y: 0.0, side: SIDE },
            Square { x: 0.0, y: 230.0, side: SIDE },
            Square { x: 310.0, y: 230.0, side: SIDE },
        ]
    }

    fn view() -> View {
        View {
            enabled: true,
            squares: squares(),
            ..View::default()
        }
    }

    fn app(bridge: &HotspotBridge, scale120: u32) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(bridge.clone())
            .insert_resource(LogicalCanvasSize(Vec2::new(320.0, 240.0)))
            .insert_resource(RendererOutputScale120(scale120))
            .init_resource::<SceneContentRevision>()
            .add_systems(First, draw);
        app
    }

    fn sprites(app: &mut App) -> Vec<(Vec2, Vec3)> {
        app.world_mut()
            .query_filtered::<(&Sprite, &Transform), With<HotspotQuad>>()
            .iter(app.world())
            .map(|(sprite, transform)| (sprite.custom_size.unwrap(), transform.translation))
            .collect()
    }

    #[test]
    fn hover_frame_is_stable_and_disabled_view_draws_nothing() {
        let now = Instant::now();
        let hovered = View {
            hover: Some(3),
            ..view()
        };
        let frame = hovered.frame(now);
        assert_eq!(frame.len(), 1);
        assert_eq!(frame[0].square, squares()[3]);
        assert_eq!(
            frame,
            hovered.frame(now + Duration::from_secs(60)),
            "a settled hover never changes, so it never asks for a frame"
        );
        assert!(
            View {
                enabled: false,
                ..hovered
            }
            .frame(now)
            .is_empty()
        );
        assert!(view().frame(now).is_empty(), "nothing engaged, nothing drawn");
    }

    #[test]
    fn flash_is_a_bounded_quantised_one_shot() {
        let at = Instant::now();
        let flashed = View {
            flash: Some((0, at)),
            ..view()
        };
        let mut levels = Vec::new();
        for ms in 0..=FLASH_DURATION.as_millis() as u64 + 50 {
            let frame = flashed.frame(at + Duration::from_millis(ms));
            let level = frame.first().map_or(0, |quad| quad.level);
            if levels.last() != Some(&level) {
                levels.push(level);
            }
        }
        assert_eq!(
            levels.len(),
            usize::from(FLASH_STEPS) + 1,
            "one distinct frame per step plus the clear: {levels:?}"
        );
        assert!(levels.windows(2).all(|pair| pair[0] > pair[1]), "{levels:?}");
        assert_eq!(levels.last(), Some(&0));
        assert!(flashed.frame(at + Duration::from_secs(3600)).is_empty());
    }

    #[test]
    fn discovery_blinks_every_hotspot_in_two_states() {
        let since = Instant::now();
        let discovering = View {
            discovery: Some(since),
            ..view()
        };
        assert_eq!(discovering.frame(since).len(), 4);
        assert!(discovering.frame(since + DISCOVERY_ON).is_empty());
        assert_eq!(discovering.frame(since + DISCOVERY_PERIOD).len(), 4);
        let mut changes = 0;
        let mut last = discovering.frame(since);
        for ms in 1..=DISCOVERY_PERIOD.as_millis() as u64 * 3 {
            let frame = discovering.frame(since + Duration::from_millis(ms));
            if frame != last {
                changes += 1;
                last = frame;
            }
        }
        assert_eq!(changes, 6, "two transitions per period over three periods");
    }

    /// The idle-render property the acceptance gate checks live: once the
    /// affordance has settled, further updates advance no scene revision.
    #[test]
    fn settled_affordance_does_not_advance_scene_revision() {
        let bridge = HotspotBridge::default();
        let mut app = app(&bridge, 120);
        app.update();
        let idle = app.world().resource::<SceneContentRevision>().0;
        app.update();
        assert_eq!(app.world().resource::<SceneContentRevision>().0, idle);

        bridge.set(View {
            hover: Some(0),
            ..view()
        });
        app.update();
        let shown = app.world().resource::<SceneContentRevision>().0;
        assert_ne!(shown, idle, "showing the hover is one render");
        for _ in 0..5 {
            app.update();
        }
        assert_eq!(
            app.world().resource::<SceneContentRevision>().0,
            shown,
            "a held hover is idle"
        );
        assert_eq!(sprites(&mut app).len(), 1);

        bridge.set(view());
        app.update();
        let hidden = app.world().resource::<SceneContentRevision>().0;
        assert_ne!(hidden, shown);
        app.update();
        assert_eq!(app.world().resource::<SceneContentRevision>().0, hidden);
        assert!(sprites(&mut app).is_empty());
    }

    /// Logical units: the square is the configured side in logical units at
    /// every output scale, positioned in the logical canvas. The output
    /// camera's logical projection supplies the physical size.
    #[test]
    fn hotspot_squares_are_logical_at_every_scale() {
        for scale120 in [120, 180, 240, 300] {
            let bridge = HotspotBridge::default();
            let mut app = app(&bridge, scale120);
            bridge.set(View {
                hover: Some(3),
                ..view()
            });
            app.update();
            let drawn = sprites(&mut app);
            assert_eq!(drawn.len(), 1, "scale {scale120}");
            let (size, translation) = drawn[0];
            assert!(
                size.abs_diff_eq(Vec2::splat(SIDE as f32), 1e-3),
                "scale {scale120}: {size}"
            );
            // Bottom-right square centre, in canvas-centred world space.
            assert!(
                translation.abs_diff_eq(Vec3::new(160.0 - 5.0, -120.0 + 5.0, HOTSPOT_Z), 1e-3),
                "scale {scale120}: {translation}"
            );
            assert!(translation.z > crate::compositor_scene::CLIENT_CONTENT_Z_MAX);
        }
    }
}
