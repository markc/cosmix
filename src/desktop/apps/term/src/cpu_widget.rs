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
pub(super) fn draw_images(
    renderer: &mut iced_tiny_skia::Renderer,
    images: &[(Handle, Rectangle)],
    origin: iced::Point,
    scale: f32,
    clip: Rectangle,
) {
    let x = (origin.x * scale).round() / scale;
    let y = (origin.y * scale).round() / scale;
    for (handle, relative) in images {
        let bounds = Rectangle {
            x: relative.x + x,
            y: relative.y + y,
            ..*relative
        };
        // Preserve cumulative edges for the vendored renderer, which resolves
        // physical placement before testing for an integer-aligned 1:1 copy.
        if bounds.intersects(&clip) {
            renderer.draw_grid(handle.clone(), bounds, clip);
        }
    }
}

impl<Message, Theme> Widget<Message, Theme, iced_tiny_skia::Renderer> for Grid {
    fn size(&self) -> Size<Length> {
        Size::new(self.width, self.height)
    }
    fn layout(
        &mut self,
        _: &mut Tree,
        _: &iced_tiny_skia::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::Node::new(limits.resolve(self.width, self.height, Size::ZERO))
    }
    fn draw(
        &self,
        _: &Tree,
        renderer: &mut iced_tiny_skia::Renderer,
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

impl<'a, Message: 'a, Theme: 'a> From<Grid>
    for iced::Element<'a, Message, Theme, iced_tiny_skia::Renderer>
{
    fn from(grid: Grid) -> Self {
        Self::new(grid)
    }
}
