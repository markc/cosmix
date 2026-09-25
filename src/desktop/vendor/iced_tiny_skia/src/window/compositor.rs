use crate::core::{Color, Rectangle, Size};
use crate::graphics::compositor::{self, Information};
use crate::graphics::damage;
use crate::graphics::error::{self, Error};
use crate::graphics::{self, Shell, Viewport};
use crate::{Layer, Renderer, Settings};

use std::collections::VecDeque;
use std::num::NonZeroU32;

pub struct Compositor {
    context: softbuffer::Context<Box<dyn compositor::Display>>,
    settings: Settings,
}

pub struct Surface {
    window: softbuffer::Surface<
        Box<dyn compositor::Display>,
        Box<dyn compositor::Window>,
    >,
    clip_mask: tiny_skia::Mask,
    history: PresentHistory,
}

impl crate::graphics::Compositor for Compositor {
    type Renderer = Renderer;
    type Surface = Surface;

    async fn with_backend(
        settings: graphics::Settings,
        display: impl compositor::Display,
        _compatible_window: impl compositor::Window,
        _shell: Shell,
        backend: Option<&str>,
    ) -> Result<Self, Error> {
        match backend {
            None | Some("tiny-skia") | Some("tiny_skia") => {
                Ok(new(settings.into(), display))
            }
            Some(backend) => Err(Error::GraphicsAdapterNotFound {
                backend: "tiny-skia",
                reason: error::Reason::DidNotMatch {
                    preferred_backend: backend.to_owned(),
                },
            }),
        }
    }

    fn create_renderer(&self) -> Self::Renderer {
        Renderer::new(
            self.settings.default_font,
            self.settings.default_text_size,
        )
    }

    fn create_surface<W: compositor::Window + Clone>(
        &mut self,
        window: W,
        width: u32,
        height: u32,
    ) -> Self::Surface {
        let window = softbuffer::Surface::new(
            &self.context,
            Box::new(window.clone()) as _,
        )
        .expect("Create softbuffer surface for window");

        let mut surface = Surface {
            window,
            clip_mask: tiny_skia::Mask::new(1, 1).expect("Create clip mask"),
            history: PresentHistory::default(),
        };

        if width > 0 && height > 0 {
            self.configure_surface(&mut surface, width, height);
        }

        surface
    }

    fn configure_surface(
        &mut self,
        surface: &mut Self::Surface,
        width: u32,
        height: u32,
    ) {
        surface
            .window
            .resize(
                NonZeroU32::new(width).expect("Non-zero width"),
                NonZeroU32::new(height).expect("Non-zero height"),
            )
            .expect("Resize surface");

        surface.clip_mask =
            tiny_skia::Mask::new(width, height).expect("Create clip mask");
        surface.history = PresentHistory::default();
    }

    fn information(&self) -> Information {
        Information {
            adapter: String::from("CPU"),
            backend: String::from("tiny-skia"),
        }
    }

    fn present(
        &mut self,
        renderer: &mut Self::Renderer,
        surface: &mut Self::Surface,
        viewport: &Viewport,
        background_color: Color,
        on_pre_present: impl FnOnce(),
    ) -> Result<(), compositor::SurfaceError> {
        present(
            renderer,
            surface,
            viewport,
            background_color,
            on_pre_present,
        )
    }

    fn screenshot(
        &mut self,
        renderer: &mut Self::Renderer,
        viewport: &Viewport,
        background_color: Color,
    ) -> Vec<u8> {
        screenshot(renderer, viewport, background_color)
    }
}

pub fn new(
    settings: Settings,
    display: impl compositor::Display,
) -> Compositor {
    #[allow(unsafe_code)]
    let context = softbuffer::Context::new(Box::new(display) as _)
        .expect("Create softbuffer context");

    Compositor { context, settings }
}

pub fn present(
    renderer: &mut Renderer,
    surface: &mut Surface,
    viewport: &Viewport,
    background_color: Color,
    on_pre_present: impl FnOnce(),
) -> Result<(), compositor::SurfaceError> {
    let physical_size = viewport.physical_size();

    let mut buffer = surface
        .window
        .buffer_mut()
        .map_err(|_| compositor::SurfaceError::Lost)?;

    let damage = surface.history.damage(
        buffer.age(), renderer.layers(), viewport, background_color,
    );
    let physical_damage = physical_damage(&damage, viewport);
    {
        let mut pixels = tiny_skia::PixmapMut::from_bytes(
            bytemuck::cast_slice_mut(&mut buffer),
            physical_size.width,
            physical_size.height,
        )
        .expect("Create pixel map");

        renderer.draw(
            &mut pixels,
            &mut surface.clip_mask,
            viewport,
            &damage,
            background_color,
        );
    }

    surface.history.submit(renderer.layers(), background_color, on_pre_present, || {
        buffer.present_with_damage(&physical_damage)
            .map_err(|_| compositor::SurfaceError::Lost)
    })
}

#[derive(Default)]
#[doc(hidden)]
// Exposed so downstream headless tests exercise the same age repair and
// submission lifecycle as the window compositor, without a second model.
pub struct PresentHistory {
    layers: VecDeque<Vec<Layer>>,
    background: Option<Color>,
    max_age: u8,
}

impl PresentHistory {
    pub fn damage(&mut self, age: u8, layers: &[Layer], viewport: &Viewport, background: Color) -> Vec<Rectangle> {
        self.max_age = self.max_age.max(age);
        self.layers.truncate(self.max_age as usize);
        let full = Rectangle::with_size(viewport.logical_size());
        let previous = age.checked_sub(1).and_then(|age| self.layers.get(age as usize));
        let diff = |old: &[Layer]| damage::diff(old, layers, |layer| vec![layer.bounds], Layer::damage);
        let mut repair = previous.filter(|_| self.background == Some(background))
            .map(|old| diff(old)).unwrap_or_else(|| vec![full]);
        // A -> B -> A still changes the displayed frame even if the acquired
        // buffer already contains A.
        if let Some(front) = self.layers.front() {
            repair.extend(diff(front));
        }
        damage::group(repair, full)
    }

    pub fn submit<E>(&mut self, layers: &[Layer], background: Color, pre_present: impl FnOnce(), present: impl FnOnce() -> Result<(), E>) -> Result<(), E> {
        // Even empty damage commits: winit's pre-present hook requests the
        // Wayland frame callback. Skipping either half removes vsync pacing
        // from unchanged NextFrame animations. Empty commits rotate ages too.
        pre_present();
        present()?;
        if self.background != Some(background) {
            self.layers.clear();
        }
        self.layers.push_front(layers.to_vec());
        self.background = Some(background);
        Ok(())
    }
}

#[doc(hidden)]
pub fn physical_damage(damage: &[Rectangle], viewport: &Viewport) -> Vec<softbuffer::Rect> {
    let size = viewport.physical_size();
    damage
        .iter()
        .filter_map(|rect| {
            let rect = *rect * viewport.scale_factor();
            let x = rect.x.floor().clamp(0.0, size.width as f32) as u32;
            let y = rect.y.floor().clamp(0.0, size.height as f32) as u32;
            let right = (rect.x + rect.width)
                .ceil()
                .clamp(x as f32, size.width as f32) as u32;
            let bottom = (rect.y + rect.height)
                .ceil()
                .clamp(y as f32, size.height as f32) as u32;
            Some(softbuffer::Rect {
                x,
                y,
                width: NonZeroU32::new(right - x)?,
                height: NonZeroU32::new(bottom - y)?,
            })
        })
        .collect()
}

pub fn screenshot(
    renderer: &mut Renderer,
    viewport: &Viewport,
    background_color: Color,
) -> Vec<u8> {
    let size = viewport.physical_size();

    let mut offscreen_buffer: Vec<u32> =
        vec![0; size.width as usize * size.height as usize];

    let mut clip_mask = tiny_skia::Mask::new(size.width, size.height)
        .expect("Create clip mask");

    renderer.draw(
        &mut tiny_skia::PixmapMut::from_bytes(
            bytemuck::cast_slice_mut(&mut offscreen_buffer),
            size.width,
            size.height,
        )
        .expect("Create offscreen pixel map"),
        &mut clip_mask,
        viewport,
        &[Rectangle::with_size(Size::new(
            size.width as f32,
            size.height as f32,
        ))],
        background_color,
    );

    offscreen_buffer.iter().fold(
        Vec::with_capacity(offscreen_buffer.len() * 4),
        |mut acc, pixel| {
            const A_MASK: u32 = 0xFF_00_00_00;
            const R_MASK: u32 = 0x00_FF_00_00;
            const G_MASK: u32 = 0x00_00_FF_00;
            const B_MASK: u32 = 0x00_00_00_FF;

            let a = ((A_MASK & pixel) >> 24) as u8;
            let r = ((R_MASK & pixel) >> 16) as u8;
            let g = ((G_MASK & pixel) >> 8) as u8;
            let b = (B_MASK & pixel) as u8;

            acc.extend([r, g, b, a]);
            acc
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "image")]
    #[test]
    fn native_cell_damage_reaches_physical_present_rectangles() {
        use crate::core::{Bytes, Renderer as _};
        use crate::grid::{Damage, Grid};
        let viewport = Viewport::with_physical_size(Size::new(2250, 200), 2.5);
        let full = Rectangle::with_size(viewport.logical_size());
        let mut stamps = Damage::new(2250, 200, (25, 50)).unwrap();
        let pixels = Bytes::from([0, 0, 0, 255].repeat(2250 * 200));
        let mut renderer = Renderer::new(crate::core::Font::default(), crate::core::Pixels(13.0));
        let mut history = PresentHistory::default();
        renderer.reset(full);
        renderer.draw_grid(Grid::with_damage(pixels.clone(), &stamps).unwrap(), full, full);
        assert_eq!(history.damage(0, renderer.layers(), &viewport, Color::BLACK), [full]);
        history.submit(renderer.layers(), Color::BLACK, || {}, || Ok::<_, ()>(())).unwrap();
        stamps.mark([Rectangle { x: 1125, y: 50, width: 25, height: 50 }]);
        renderer.reset(full);
        renderer.draw_grid(Grid::with_damage(pixels, &stamps).unwrap(), full, full);
        let regions = history.damage(1, renderer.layers(), &viewport, Color::BLACK);
        let physical = physical_damage(&regions, &viewport);
        assert_eq!(physical.len(), 1);
        let rect = &physical[0];
        assert_eq!((rect.x, rect.y, rect.width.get(), rect.height.get()), (1122, 47, 31, 56));
        // Identical scene: no surface damage, even with the changed generation.
        history.submit(renderer.layers(), Color::BLACK, || {}, || Ok::<_, ()>(())).unwrap();
        assert!(history.damage(1, renderer.layers(), &viewport, Color::BLACK).is_empty());
    }

    fn scene(x: f32) -> Vec<Layer> {
        vec![Layer {
            bounds: Rectangle { x, y: 0.0, width: 8.0, height: 8.0 },
            quads: vec![], primitives: vec![], images: vec![], text: vec![],
        }]
    }

    #[test]
    fn present_lifecycle_repairs_ages_and_paces_empty_frames() {
        let viewport = Viewport::with_physical_size(Size::new(100, 80), 1.25);
        let full = Rectangle::with_size(viewport.logical_size());
        let mut history = PresentHistory::default();
        let a = scene(0.0);
        let b = scene(20.0);
        let submit = |history: &mut PresentHistory, layers: &[Layer], colour| {
            let calls = std::cell::Cell::new(0);
            history.submit(layers, colour,
                || calls.set(1),
                || { assert_eq!(calls.get(), 1); calls.set(2); Ok::<_, ()>(()) }).unwrap();
            assert_eq!(calls.get(), 2, "even empty damage must commit after the hook");
        };
        assert_eq!(history.damage(0, &a, &viewport, Color::BLACK), [full]);
        submit(&mut history, &a, Color::BLACK);
        assert!(history.damage(1, &a, &viewport, Color::BLACK).is_empty());
        submit(&mut history, &a, Color::BLACK);
        assert_eq!(history.layers.len(), 2, "empty present advances history");
        assert!(!history.damage(2, &b, &viewport, Color::BLACK).is_empty());
        submit(&mut history, &b, Color::BLACK);
        assert!(!history.damage(2, &a, &viewport, Color::BLACK).is_empty(), "A -> B -> A damages front");
        submit(&mut history, &a, Color::BLACK);
        assert_eq!(history.damage(9, &a, &viewport, Color::BLACK), [full]);
        assert_eq!(history.damage(1, &a, &viewport, Color::WHITE), [full]);
        submit(&mut history, &a, Color::WHITE);
        assert_eq!(history.layers.len(), 1, "old clear colours cannot be reused");
        assert_eq!(history.damage(2, &a, &viewport, Color::WHITE), [full]);
        // configure_surface resets this state on resize AND output-scale change.
        history = PresentHistory::default();
        let resized = Viewport::with_physical_size(Size::new(120, 90), 1.5);
        assert_eq!(history.damage(1, &a, &resized, Color::WHITE),
            [Rectangle::with_size(resized.logical_size())]);
        assert!(history.submit(&a, Color::WHITE, || {}, || Err(())).is_err());
        assert!(history.layers.is_empty(), "failed presents never enter history");
    }

    #[test]
    fn damage_is_outward_rounded_clamped_and_empty_stays_empty() {
        let viewport = Viewport::with_physical_size(Size::new(100, 80), 1.25);
        assert!(physical_damage(&[], &viewport).is_empty());
        let rects = physical_damage(
            &[
                Rectangle {
                    x: 1.0,
                    y: 2.0,
                    width: 3.0,
                    height: 4.0,
                },
                Rectangle {
                    x: -2.0,
                    y: 60.0,
                    width: 90.0,
                    height: 20.0,
                },
            ],
            &viewport,
        );
        let values: Vec<_> = rects
            .iter()
            .map(|r| (r.x, r.y, r.width.get(), r.height.get()))
            .collect();
        assert_eq!(values, [(1, 2, 4, 6), (0, 75, 100, 5)]);
    }
}
