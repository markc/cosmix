use super::*;
use iced::advanced::{Layout, Widget, layout, mouse, renderer, widget::Tree};
use iced::{Length, Rectangle, Size};

pub struct Grid {
    images: Vec<(Handle, Rectangle)>,
    scale: f32,
    width: Length,
    height: Length,
}

pub fn view(frame: &Arc<Mutex<Frame>>, scale: f32) -> Grid {
    Grid {
        images: frame.lock().expect("frame lock").surface().images(scale),
        scale,
        width: Length::Shrink,
        height: Length::Shrink,
    }
}

impl Grid {
    pub fn width(mut self, width: Length) -> Self {
        self.width = width;
        self
    }
    pub fn height(mut self, height: Length) -> Self {
        self.height = height;
        self
    }
}

/// Shared by the widget and benchmark. Bounds are physical extents divided
/// by output scale, never by available layout space. Snap only the origin;
/// deriving every band's origin from integer pixels prevents fractional seams.
pub(super) fn draw_images<R: iced::advanced::image::Renderer<Handle = Handle>>(
    renderer: &mut R,
    images: &[(Handle, Rectangle)],
    origin: iced::Point,
    scale: f32,
    clip: Rectangle,
) {
    let x = (origin.x * scale).round() / scale;
    let y = (origin.y * scale).round() / scale;
    for (handle, relative) in images {
        // tiny-skia trims ids not touched in a draw. Touch ALL visible bands,
        // including undamaged ones: otherwise an adjacent damage rectangle
        // can repeatedly re-convert an unchanged band after cache eviction.
        let Some(size) = renderer.measure_image(handle) else {
            continue;
        };
        let mut bounds = Rectangle {
            x: relative.x + x,
            y: relative.y + y,
            ..*relative
        };
        // iced_tiny_skia divides these coordinates by the image's logical
        // pixel size, then casts to i32 (truncating). A mathematically integral
        // position can land just below the integer at fractional scale. Put
        // the quotient halfway inside the intended truncation interval using
        // the SAME pixel size as iced. This is only a placement correction:
        // keep the shared-edge extents unchanged, never accumulate the bias.
        bounds.x = truncating_origin(bounds.x, scale, bounds.width / size.width as f32);
        bounds.y = truncating_origin(bounds.y, scale, bounds.height / size.height as f32);
        if bounds.intersects(&clip) {
            let mut image = iced::advanced::image::Image::new(handle.clone());
            image.filter_method = image::FilterMethod::Nearest;
            renderer.draw_image(image, bounds, clip);
        }
    }
}

fn truncating_origin(logical: f32, scale: f32, pixel_size: f32) -> f32 {
    let physical = (logical * scale).round();
    (physical + 0.5_f32.copysign(physical)) * pixel_size
}

impl<Message, Theme, R> Widget<Message, Theme, R> for Grid
where
    R: iced::advanced::image::Renderer<Handle = Handle>,
{
    fn size(&self) -> Size<Length> {
        Size::new(self.width, self.height)
    }
    fn layout(&mut self, _: &mut Tree, _: &R, limits: &layout::Limits) -> layout::Node {
        layout::Node::new(limits.resolve(self.width, self.height, Size::ZERO))
    }
    fn draw(
        &self,
        _: &Tree,
        renderer: &mut R,
        _: &Theme,
        _: &renderer::Style,
        layout: Layout<'_>,
        _: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        if let Some(clip) = layout.bounds().intersection(viewport) {
            draw_images(renderer, &self.images, layout.position(), self.scale, clip);
        }
    }
}

impl<'a, Message: 'a, Theme: 'a, R> From<Grid> for iced::Element<'a, Message, Theme, R>
where
    R: iced::advanced::image::Renderer<Handle = Handle> + 'a,
{
    fn from(grid: Grid) -> Self {
        Self::new(grid)
    }
}
