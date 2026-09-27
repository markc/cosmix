//! Filename middle elision, ported from CTK's `text_elide` without Bevy.
use iced::advanced::text::{self, Paragraph as _};
use iced::{Element, Length, Size};
use iced_tiny_skia::Renderer;
use unicode_segmentation::UnicodeSegmentation;

type Para = <Renderer as text::Renderer>::Paragraph;

pub fn shape(content: &str, font: iced::Font, px: f32) -> Para {
    Para::with_text(text::Text {
        content,
        bounds: Size::INFINITE,
        size: px.into(),
        line_height: text::LineHeight::default(),
        font,
        align_x: text::Alignment::Left,
        align_y: iced::alignment::Vertical::Top,
        shaping: text::Shaping::Basic,
        wrapping: text::Wrapping::None,
    })
}

/// Same extension rule and bounded measured search as filemgr's MiddleElideText.
/// Width is measured with the actual renderer/font, never character estimates.
pub fn middle(text: &str, width: f32, mut measure: impl FnMut(&str) -> f32) -> String {
    if text.is_empty() || !width.is_finite() || width <= 0.0 {
        return String::new();
    }
    if measure(text) <= width {
        return text.to_owned();
    }
    if measure("…") > width {
        return String::new();
    }
    let boundaries: Vec<_> = text
        .grapheme_indices(true)
        .map(|(i, _)| i)
        .chain(std::iter::once(text.len()))
        .collect();
    let count = boundaries.len() - 1;
    if count <= 1 {
        return "…".into();
    }
    let extension = text
        .rfind('.')
        .filter(|i| *i > 0 && *i + 1 < text.len())
        .and_then(|i| boundaries.iter().position(|b| *b == i));
    let stem = extension.unwrap_or(count - 1);
    let suffix = extension.map_or(1, |i| count - i);
    let mut measures = 2;
    for keep in (1..=suffix.min(12)).rev() {
        let (mut low, mut high, mut best) = (0, stem, None);
        while low <= high {
            if measures >= 32 {
                return "…".into();
            }
            measures += 1;
            let prefix = low + (high - low) / 2;
            let candidate = if suffix > 12 {
                format!(
                    "{}….{}",
                    &text[..boundaries[prefix]],
                    &text[boundaries[count - keep + 1]..]
                )
            } else {
                format!(
                    "{}…{}",
                    &text[..boundaries[prefix]],
                    &text[boundaries[count - keep]..]
                )
            };
            if measure(&candidate) <= width {
                best = Some(candidate);
                low = prefix + 1;
            } else if prefix == 0 {
                break;
            } else {
                high = prefix - 1;
            }
        }
        if let Some(best) = best {
            return best;
        }
    }
    "…".into()
}

/// A fill-width, clipped label for Places, Properties and paths. Elision is
/// resolved during layout, so resize and typography changes cannot go stale.
pub struct Label {
    pub text: String,
    pub font: iced::Font,
    pub px: f32,
    pub color: iced::Color,
}
impl<M> iced::advanced::Widget<M, iced::Theme, Renderer> for Label {
    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Shrink)
    }
    fn state(&self) -> iced::advanced::widget::tree::State {
        iced::advanced::widget::tree::State::new(shape("", self.font, self.px))
    }
    fn layout(
        &mut self,
        tree: &mut iced::advanced::widget::Tree,
        _: &Renderer,
        limits: &iced::advanced::layout::Limits,
    ) -> iced::advanced::layout::Node {
        let width = limits.max().width;
        let value = middle(&self.text, width, |s| {
            shape(s, self.font, self.px).min_bounds().width
        });
        let para = shape(&value, self.font, self.px);
        let height = shape("Ag", self.font, self.px).min_bounds().height;
        *tree.state.downcast_mut::<Para>() = para;
        iced::advanced::layout::Node::new(limits.resolve(
            Length::Fill,
            Length::Shrink,
            Size::new(width, height),
        ))
    }
    fn draw(
        &self,
        tree: &iced::advanced::widget::Tree,
        renderer: &mut Renderer,
        _: &iced::Theme,
        _: &iced::advanced::renderer::Style,
        layout: iced::advanced::Layout<'_>,
        _: iced::mouse::Cursor,
        viewport: &iced::Rectangle,
    ) {
        use iced::advanced::{Renderer as _, text::Renderer as _};
        if let Some(clip) = layout.bounds().intersection(viewport) {
            renderer.with_layer(clip, |renderer| {
                renderer.fill_paragraph(
                    tree.state.downcast_ref::<Para>(),
                    layout.bounds().position(),
                    self.color,
                    clip,
                )
            });
        }
    }
}
impl<'a, M: 'a> From<Label> for Element<'a, M, iced::Theme, Renderer> {
    fn from(label: Label) -> Self {
        Element::new(label)
    }
}
